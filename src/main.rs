#![cfg_attr(target_arch = "arm", no_std)]
#![cfg_attr(target_arch = "arm", no_main)]

#[cfg(not(target_arch = "arm"))]
fn main() {
    println!("Build this firmware for thumbv7em-none-eabihf");
}

#[cfg(target_arch = "arm")]
extern crate alloc;

#[cfg(target_arch = "arm")]
mod firmware {
    use alloc::format;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use core::fmt::Write as _;
    use core::mem;

    use embedded_alloc::LlffHeap as Heap;
    use embedded_graphics::mono_font::ascii::FONT_10X20;
    use embedded_hal::digital::OutputPin;
    use embedded_hal::spi::{ErrorType as SpiErrorType, Operation, SpiBus, SpiDevice};
    use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
    use mousefood::{EmbeddedBackend, EmbeddedBackendConfig};
    use panic_halt as _;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block as UiBlock, Clear, Paragraph, Wrap};
    use ratatui::{Frame, Terminal};
    use wio_terminal as wio;

    use wio::entry;
    use wio::hal::clock::GenericClockController;
    use wio::hal::delay::Delay;
    use wio::hal::gpio::{DynPin, DYN_PULL_UP_INPUT, DYN_PUSH_PULL_OUTPUT};
    use wio::hal::rtc;
    use wio::hal::sercom::spi;
    use wio::pac::{CorePeripherals, Peripherals};
    use wio::prelude::*;

    use wio_terminal_sd_editor::{
        format_fat32, is_txt_file, join_path, leaf_name, parent_path, probe_fat,
        save_failure_reason, sd_retry, validate_entry_name, validate_file_stem, Button, CardFs,
        DirectoryItem, EditError, FsOpError, InputEngine, Key, Keyboard, ProbeError,
        RawButtons, SdStream, TextBuffer, EXPLORER_PAGE_ROWS, MAX_DOCUMENT_BYTES,
        MAX_ENTRY_NAME_CHARS, MAX_FILE_STEM_CHARS, OLD_ENTRY_REMAINS, SD_RETRY_ATTEMPTS,
    };

    const HEAP_SIZE: usize = 112 * 1024;
    const POLL_TICKS: u32 = 21;
    const BATTERY_POLL_TICKS: u32 = 2 * 1024;
    const FORMAT_HOLD_TICKS: u32 = 2 * 1024;
    // ~250 ms at 1024 Hz, applied to insertion only. A card slides past the
    // detect switch before its contacts are seated, so mounting on the first
    // sampled edge can fail mid-insertion. Removal is acted on immediately:
    // every sampled absence unmounts and marks the card uninitialized, so no
    // write can be dispatched to a card that is leaving the slot.
    const CARD_SETTLE_TICKS: u32 = 256;
    const BQ27441_ADDRESS: u8 = 0x55;
    const BQ27441_AVERAGE_CURRENT: u8 = 0x10;
    const BQ27441_STATE_OF_CHARGE: u8 = 0x1c;
    // Delay counts are CPU cycles, not loop iterations. 600 cycles is at
    // least 5 us at the Wio Terminal's 120 MHz core clock, which satisfies
    // the BQ27441's standard-mode SCL high/low timing requirements.
    const BATTERY_I2C_HALF_PERIOD_CYCLES: u32 = 600;
    // The gauge requires 66 us of bus-free time between addressed packets.
    const BATTERY_I2C_BUS_FREE_CYCLES: u32 = 8_000;
    // The BQ27441 can stretch SCL for up to 4 ms. At the Wio Terminal's
    // 120 MHz core clock, this many polls guarantees a timeout of at least
    // 5 ms even if an optimized poll took only one cycle.
    const BATTERY_I2C_STRETCH_POLLS: usize = 600_000;

    #[global_allocator]
    static HEAP: Heap = Heap::empty();

    /// `embedded-sdmmc` performs one `SpiDevice` transaction for each
    /// piece of an SD command. The Wio BSP's `ExclusiveDevice` consequently
    /// raises CS between a write's token, payload, CRC, and response. Some
    /// cards tolerate those pulses while reading but correctly reject writes.
    /// SERCOM6 is dedicated to the card, so keep it selected for the whole
    /// protocol exchange instead.
    struct SelectedSdDevice {
        bus: wio::SdSpi,
        cs: wio::aliases::SdCs,
    }

    impl SelectedSdDevice {
        const TRANSFER_CHUNK_BYTES: usize = 32;

        fn new(bus: wio::SdSpi, cs: wio::aliases::SdCs) -> Result<Self, ()> {
            let mut device = Self { bus, cs };
            device.prepare_card()?;
            Ok(device)
        }

        /// Send bytes while draining the receive side of the full-duplex
        /// SERCOM peripheral.
        ///
        /// `atsamd-hal` implements `SpiBus::write` by disabling reception, but
        /// reenables it before the final byte has completely shifted out. That
        /// can leave the byte received alongside the final transmitted byte in
        /// the peripheral. The next one-byte transfer then returns that stale
        /// value instead of the SD card's data-response token, making every
        /// otherwise valid block write look rejected. Full-duplex transfers
        /// keep the receiver drained and the subsequent response aligned.
        fn write_discarding_read(
            &mut self,
            bytes: &[u8],
        ) -> Result<(), <wio::SdSpi as SpiErrorType>::Error> {
            let mut scratch = [0u8; Self::TRANSFER_CHUNK_BYTES];
            for chunk in bytes.chunks(Self::TRANSFER_CHUNK_BYTES) {
                scratch[..chunk.len()].copy_from_slice(chunk);
                SpiBus::transfer_in_place(&mut self.bus, &mut scratch[..chunk.len()])?;
            }
            Ok(())
        }

        /// SD cards require at least 74 clocks with CS high before CMD0.
        /// Finish with CS low; all subsequent bus operations belong to this
        /// card until the next removal/insertion cycle.
        fn prepare_card(&mut self) -> Result<(), ()> {
            OutputPin::set_high(&mut self.cs).map_err(|_| ())?;
            self.write_discarding_read(&[0xff; 10]).map_err(|_| ())?;
            SpiBus::flush(&mut self.bus).map_err(|_| ())?;
            OutputPin::set_low(&mut self.cs).map_err(|_| ())
        }

        fn bus_mut(&mut self) -> &mut wio::SdSpi {
            &mut self.bus
        }
    }

    impl SpiErrorType for SelectedSdDevice {
        type Error = <wio::SdSpi as SpiErrorType>::Error;
    }

    impl SpiDevice<u8> for SelectedSdDevice {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
            for operation in operations {
                match operation {
                    Operation::Read(buffer) => SpiBus::read(&mut self.bus, buffer)?,
                    Operation::Write(buffer) => self.write_discarding_read(buffer)?,
                    Operation::Transfer(read, write) => {
                        SpiBus::transfer(&mut self.bus, read, write)?
                    }
                    Operation::TransferInPlace(buffer) => {
                        SpiBus::transfer_in_place(&mut self.bus, buffer)?
                    }
                    // The SD driver does not request in-transaction delays.
                    Operation::DelayNs(_) => SpiBus::flush(&mut self.bus)?,
                }
            }
            SpiBus::flush(&mut self.bus)
        }
    }

    type Controller = embedded_sdmmc::SdCard<SelectedSdDevice, Delay>;

    #[derive(Clone)]
    struct ControllerDevice {
        controller: Rc<RefCell<Controller>>,
        /// Latched when a full retry cycle (attempts plus recoveries) ends
        /// in failure: the card is almost certainly gone or dead, and
        /// paying three attempts plus two lazy ~400 kHz re-inits per block
        /// would freeze the event loop for minutes during a directory
        /// walk. While set, each operation makes ONE bare attempt (no
        /// retries, no recovery) and any success clears the latch. Shared
        /// across clones; card-detect remounts build fresh devices, which
        /// is what resets it for a new card.
        dead: Rc<Cell<bool>>,
    }

    impl ControllerDevice {
        fn new(controller: Rc<RefCell<Controller>>) -> Self {
            Self {
                controller,
                dead: Rc::new(Cell::new(false)),
            }
        }

        fn recover_card(&self) {
            let controller = self.controller.borrow();
            let _ = controller.spi(|device| device.prepare_card());
            controller.mark_card_uninit();
        }

        // Single SPI exchanges flake on real cards, so every access gets
        // the standard retry policy. Recovery between attempts re-preps
        // and re-initializes the card, which is what lets the next attempt
        // succeed; a card that survives a whole cycle of that is declared
        // dead (see the `dead` field) instead of being recovered one last
        // time, so the next operation fails fast.
        fn run<T>(
            &self,
            mut op: impl FnMut(&Controller) -> Result<T, embedded_sdmmc::SdCardError>,
        ) -> Result<T, embedded_sdmmc::SdCardError> {
            if self.dead.get() {
                let result = op(&self.controller.borrow());
                if result.is_ok() {
                    self.dead.set(false);
                }
                return result;
            }
            let mut attempt = 0usize;
            let result = sd_retry(|| {
                attempt += 1;
                let result = op(&self.controller.borrow());
                // Recover only *between* attempts: after the final failure
                // the card is declared dead and must fail fast, not be left
                // marked-uninit to pay a full slow-clock lazy re-init on
                // every bare attempt while the latch holds.
                if result.is_err() && attempt < SD_RETRY_ATTEMPTS {
                    self.recover_card();
                }
                result
            });
            if result.is_err() {
                self.dead.set(true);
            }
            result
        }
    }

    impl BlockDevice for ControllerDevice {
        type Error = embedded_sdmmc::SdCardError;

        fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
            self.run(|controller| controller.read(blocks, start))
        }

        fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
            self.run(|controller| {
                let result = controller.write(blocks, start);
                // Some cards return a late status error after accepting the
                // data. The filesystem issues one block at a time, so verify
                // that block before resetting and repeating an operation that
                // may already have won.
                if result.is_err() && blocks.len() == 1 {
                    let mut actual = Block::new();
                    if controller
                        .read(core::slice::from_mut(&mut actual), start)
                        .is_ok()
                        && actual.contents == blocks[0].contents
                    {
                        return Ok(());
                    }
                }
                result
            })
        }

        fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
            self.run(|controller| controller.num_blocks())
        }
    }

    type Fs = CardFs<ControllerDevice>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MediaIdentity {
        sectors: u32,
        partition_start: u32,
        volume_serial: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum MountFailure {
        Io,
        Unsupported,
        Invalid,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EditorMedia {
        Ready,
        Removed,
        Different,
    }

    struct Explorer {
        path: String,
        offset: usize,
        selected: usize,
        entries: Vec<DirectoryItem>,
        total: usize,
        free_bytes: Option<u64>,
        status: Option<String>,
    }

    impl Explorer {
        fn root() -> Self {
            Self {
                path: "/".into(),
                offset: 0,
                selected: 0,
                entries: Vec::new(),
                total: 0,
                free_bytes: None,
                status: None,
            }
        }

        fn refresh(&mut self, fs: &Fs) {
            self.free_bytes = fs.free_space_bytes();
            match fs.read_directory_page(&self.path, self.offset, EXPLORER_PAGE_ROWS) {
                Ok((entries, total)) => {
                    self.entries = entries;
                    self.total = total;
                    if self.entries.is_empty() {
                        self.selected = 0;
                        if self.offset != 0 {
                            self.offset = self.offset.saturating_sub(EXPLORER_PAGE_ROWS);
                            self.refresh(fs);
                        }
                    } else {
                        self.selected = self.selected.min(self.entries.len() - 1);
                    }
                    self.status = None;
                }
                Err(_) => {
                    self.entries.clear();
                    self.total = 0;
                    self.status = Some("Could not read this folder".into());
                }
            }
        }

        fn go_parent(&mut self, fs: &Fs) {
            if self.path != "/" {
                self.path = parent_path(&self.path);
                self.offset = 0;
                self.selected = 0;
                self.refresh(fs);
            }
        }

        fn refresh_select_path(&mut self, fs: &Fs, target: &str) {
            if let Ok(Some(index)) = fs.entry_index(&self.path, target) {
                self.offset = (index / EXPLORER_PAGE_ROWS) * EXPLORER_PAGE_ROWS;
                self.selected = index - self.offset;
            }
            self.refresh(fs);
        }
    }

    #[derive(Clone, Copy)]
    enum NameMode {
        NewText,
        NewFolder,
        Rename,
    }

    struct NameEntry {
        explorer: Explorer,
        mode: NameMode,
        source: Option<DirectoryItem>,
        name: String,
        cursor: usize,
        keyboard: Keyboard,
        keyboard_visible: bool,
        status: Option<String>,
    }

    impl NameEntry {
        fn can_insert(&self, _ch: char) -> bool {
            match self.mode {
                // Folders (like renames) may carry a dot and extension; the
                // shorter stem cap is for new .txt files alone. Character
                // validity is enforced by validate_entry_name at DONE time.
                NameMode::NewFolder | NameMode::Rename => {
                    self.name.chars().count() < MAX_ENTRY_NAME_CHARS
                }
                NameMode::NewText => self.name.chars().count() < MAX_FILE_STEM_CHARS,
            }
        }
    }

    #[derive(Clone, Copy)]
    enum NewChoice {
        TextFile,
        Folder,
    }

    impl NewChoice {
        fn next(self) -> Self {
            match self {
                Self::TextFile => Self::Folder,
                Self::Folder => Self::TextFile,
            }
        }
    }

    struct NewMenu {
        explorer: Explorer,
        choice: NewChoice,
    }

    #[derive(Clone, Copy)]
    enum ActionChoice {
        Rename,
        Move,
        Delete,
        Refresh,
    }

    impl ActionChoice {
        fn previous(self) -> Self {
            match self {
                Self::Rename => Self::Refresh,
                Self::Move => Self::Rename,
                Self::Delete => Self::Move,
                Self::Refresh => Self::Delete,
            }
        }

        fn next(self) -> Self {
            match self {
                Self::Rename => Self::Move,
                Self::Move => Self::Delete,
                Self::Delete => Self::Refresh,
                Self::Refresh => Self::Rename,
            }
        }
    }

    struct ActionMenu {
        explorer: Explorer,
        item: Option<DirectoryItem>,
        choice: ActionChoice,
    }

    struct MovePicker {
        origin: Explorer,
        source: DirectoryItem,
        path: String,
        offset: usize,
        selected: usize,
        entries: Vec<DirectoryItem>,
        total: usize,
        status: Option<String>,
    }

    impl MovePicker {
        fn new(origin: Explorer, source: DirectoryItem) -> Self {
            let path = parent_path(&source.path);
            Self {
                origin,
                source,
                path,
                offset: 0,
                selected: 0,
                entries: Vec::new(),
                total: 0,
                status: None,
            }
        }

        fn refresh(&mut self, fs: &Fs) {
            let excluded = self.source.is_dir.then_some(self.source.path.as_str());
            match fs.read_directory_folders_page(
                &self.path,
                self.offset,
                EXPLORER_PAGE_ROWS,
                excluded,
            ) {
                Ok((entries, total)) => {
                    self.entries = entries;
                    self.total = total;
                    if self.entries.is_empty() {
                        self.selected = 0;
                        if self.offset != 0 {
                            self.offset = self.offset.saturating_sub(EXPLORER_PAGE_ROWS);
                            self.refresh(fs);
                        }
                    } else {
                        self.selected = self.selected.min(self.entries.len() - 1);
                    }
                    self.status = None;
                }
                Err(_) => {
                    self.entries.clear();
                    self.total = 0;
                    self.status = Some("Could not read this folder".into());
                }
            }
        }

        fn go_parent(&mut self, fs: &Fs) {
            if self.path != "/" {
                self.path = parent_path(&self.path);
                self.offset = 0;
                self.selected = 0;
                self.refresh(fs);
            }
        }
    }

    /// A copy-based filesystem operation slow enough to warrant a blocking
    /// "working" notice: `handle_button` returns it as [`HandleResult::Op`],
    /// and the event loop paints the notice before running it synchronously
    /// via `run_fs_op` (same pattern as Mounting/Formatting).
    enum PendingFsOp {
        Rename {
            entry: NameEntry,
            destination: String,
        },
        Move {
            picker: MovePicker,
            destination: String,
        },
        Delete {
            explorer: Explorer,
            item: DirectoryItem,
        },
    }

    impl PendingFsOp {
        fn title(&self) -> &'static str {
            match self {
                Self::Rename { .. } => "RENAMING",
                Self::Move { .. } => "MOVING",
                Self::Delete { .. } => "DELETING",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum DeleteChoice {
        Cancel,
        Delete,
    }

    struct Editor {
        explorer: Explorer,
        path: String,
        name: String,
        buffer: TextBuffer,
        keyboard: Keyboard,
        keyboard_visible: bool,
        scroll_line: usize,
        horizontal: usize,
        status: Option<String>,
        media: EditorMedia,
        identity: MediaIdentity,
    }

    impl Editor {
        fn ensure_cursor_visible(&mut self) {
            let visible_lines = if self.keyboard_visible { 5 } else { 9 };
            let (line, column) = self.buffer.display_position();
            if line < self.scroll_line {
                self.scroll_line = line;
            } else if line >= self.scroll_line + visible_lines {
                self.scroll_line = line + 1 - visible_lines;
            }
            if column < self.horizontal {
                self.horizontal = column;
            } else if column >= self.horizontal + 30 {
                self.horizontal = column + 1 - 30;
            }
        }
    }

    #[derive(Clone, Copy)]
    enum ExitChoice {
        Save,
        Discard,
        Cancel,
    }

    impl ExitChoice {
        fn left(self) -> Self {
            match self {
                Self::Save => Self::Cancel,
                Self::Discard => Self::Save,
                Self::Cancel => Self::Discard,
            }
        }

        fn right(self) -> Self {
            match self {
                Self::Save => Self::Discard,
                Self::Discard => Self::Cancel,
                Self::Cancel => Self::Save,
            }
        }
    }

    enum Screen {
        Missing,
        Mounting,
        FormatPrompt {
            reason: &'static str,
            /// Set by TOP LEFT; disarms the confirmation hold until TOP
            /// RIGHT re-arms it.
            cancelled: bool,
            /// RTC tick when this prompt appeared. The confirmation hold only
            /// counts presses that began at or after this moment.
            opened_at: u32,
        },
        Formatting,
        Explorer(Explorer),
        NewMenu(NewMenu),
        ActionMenu(ActionMenu),
        Naming(NameEntry),
        MovePicker(MovePicker),
        DeletePrompt {
            explorer: Explorer,
            item: DirectoryItem,
            choice: DeleteChoice,
        },
        Editor(Editor),
        ExitPrompt {
            editor: Editor,
            choice: ExitChoice,
        },
        Fatal(&'static str),
    }

    /// What a button press produced: the next screen, or a slow filesystem
    /// operation the event loop must paint a notice for before running it
    /// synchronously. Keeping pending work out of [`Screen`] makes a
    /// "working" notice that outlives its operation unrepresentable.
    enum HandleResult {
        Screen(Screen),
        Op(PendingFsOp),
    }

    impl From<Screen> for HandleResult {
        fn from(screen: Screen) -> Self {
            Self::Screen(screen)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct BatteryStatus {
        percent: u8,
        charging: bool,
    }

    /// Small software I2C master for the optional battery chassis.
    ///
    /// The HAL's blocking SERCOM implementation waits indefinitely for some
    /// bus states. That is unsuitable for a removable accessory: a chassis
    /// can be attached while its gauge is asleep or holding a line low. Every
    /// wait here is bounded so battery telemetry can never stall SD handling.
    struct BatteryI2c {
        sda: DynPin,
        scl: DynPin,
    }

    impl BatteryI2c {
        fn new(mut sda: DynPin, mut scl: DynPin) -> Self {
            sda.into_mode(DYN_PULL_UP_INPUT);
            scl.into_mode(DYN_PULL_UP_INPUT);
            Self { sda, scl }
        }

        fn read_word(&mut self, register: u8) -> Option<u16> {
            self.start()?;
            let result = (|| {
                if !self.write_byte(BQ27441_ADDRESS << 1)? || !self.write_byte(register)? {
                    return None;
                }

                // The BQ27441 register-read protocol requires a repeated
                // START here. A STOP would separate the command and data
                // phases, so the requested register would not be read.
                self.start()?;
                if !self.write_byte((BQ27441_ADDRESS << 1) | 1)? {
                    return None;
                }
                let low = self.read_byte(true)?;
                let high = self.read_byte(false)?;
                Some(u16::from_le_bytes([low, high]))
            })();
            self.stop();
            result
        }

        fn start(&mut self) -> Option<()> {
            self.release_sda();
            // A repeated START follows the command byte with SCL low. Keep it
            // low for a complete half-period before raising it again; raising
            // it immediately can make the BQ27441 miss the repeated START and
            // leaves every register read looking like a disconnected chassis.
            Self::half_period();
            self.release_scl();
            self.wait_scl_high()?;
            if !self.sda.is_high().ok()? {
                self.recover();
                if !self.sda.is_high().ok()? {
                    return None;
                }
            }
            // SDA must remain high after SCL rises before START (and especially
            // repeated START) is asserted. Without this setup time the gauge
            // can interpret the following address as part of the write phase.
            Self::half_period();
            self.drive_sda_low();
            Self::half_period();
            self.drive_scl_low();
            Some(())
        }

        fn stop(&mut self) {
            self.drive_sda_low();
            Self::half_period();
            self.release_scl();
            let _ = self.wait_scl_high();
            // Meet tSU(STOP): keep SDA low long enough after SCL rises for the
            // gauge to recognize the STOP condition.
            Self::half_period();
            self.release_sda();
            cortex_m::asm::delay(BATTERY_I2C_BUS_FREE_CYCLES);
        }

        fn recover(&mut self) {
            self.release_sda();
            for _ in 0..9 {
                self.drive_scl_low();
                Self::half_period();
                self.release_scl();
                if self.wait_scl_high().is_none() {
                    return;
                }
            }
            self.stop();
        }

        fn write_byte(&mut self, byte: u8) -> Option<bool> {
            for bit in (0..8).rev() {
                if byte & (1 << bit) == 0 {
                    self.drive_sda_low();
                } else {
                    self.release_sda();
                }
                Self::half_period();
                self.release_scl();
                self.wait_scl_high()?;
                Self::half_period();
                self.drive_scl_low();
            }

            self.release_sda();
            Self::half_period();
            self.release_scl();
            self.wait_scl_high()?;
            let acknowledged = self.sda.is_low().ok()?;
            Self::half_period();
            self.drive_scl_low();
            Some(acknowledged)
        }

        fn read_byte(&mut self, acknowledge: bool) -> Option<u8> {
            self.release_sda();
            let mut byte = 0;
            for _ in 0..8 {
                Self::half_period();
                self.release_scl();
                self.wait_scl_high()?;
                byte = (byte << 1) | u8::from(self.sda.is_high().ok()?);
                Self::half_period();
                self.drive_scl_low();
            }

            if acknowledge {
                self.drive_sda_low();
            } else {
                self.release_sda();
            }
            Self::half_period();
            self.release_scl();
            self.wait_scl_high()?;
            Self::half_period();
            self.drive_scl_low();
            self.release_sda();
            Some(byte)
        }

        fn wait_scl_high(&mut self) -> Option<()> {
            for _ in 0..BATTERY_I2C_STRETCH_POLLS {
                if self.scl.is_high().ok()? {
                    return Some(());
                }
            }
            None
        }

        fn drive_sda_low(&mut self) {
            self.sda.into_mode(DYN_PUSH_PULL_OUTPUT);
            let _ = OutputPin::set_low(&mut self.sda);
        }

        fn release_sda(&mut self) {
            self.sda.into_mode(DYN_PULL_UP_INPUT);
        }

        fn drive_scl_low(&mut self) {
            self.scl.into_mode(DYN_PUSH_PULL_OUTPUT);
            let _ = OutputPin::set_low(&mut self.scl);
        }

        fn release_scl(&mut self) {
            self.scl.into_mode(DYN_PULL_UP_INPUT);
        }

        fn half_period() {
            cortex_m::asm::delay(BATTERY_I2C_HALF_PERIOD_CYCLES);
        }
    }

    #[entry]
    fn main() -> ! {
        unsafe {
            embedded_alloc::init!(HEAP, HEAP_SIZE);
        };

        let mut peripherals = Peripherals::take().unwrap();
        let core = CorePeripherals::take().unwrap();
        let mut clocks = GenericClockController::with_external_32kosc(
            peripherals.gclk,
            &mut peripherals.mclk,
            &mut peripherals.osc32kctrl,
            &mut peripherals.oscctrl,
            &mut peripherals.nvmctrl,
        );
        let mut delay = Delay::new(core.SYST, &mut clocks);
        let pins = wio::Pins::new(peripherals.port);

        // `count32_mode` alone does not enable CLOCKSYNC on SAMD51. Running
        // through the mode conversion applies the complete D5x configuration,
        // otherwise repeated count reads can remain stale and gate the UI loop
        // forever before its first frame.
        let timer = rtc::Rtc::count32_mode(peripherals.rtc, 1024.Hz(), &mut peripherals.mclk)
            .into_count32_mode();
        let display_pins = wio::Display {
            miso: pins.lcd_miso,
            mosi: pins.lcd_mosi,
            sck: pins.lcd_sck,
            cs: pins.lcd_cs,
            dc: pins.lcd_dc,
            reset: pins.lcd_reset,
            backlight: pins.lcd_backlight,
        };
        let (mut display, _backlight) = display_pins
            .init(
                &mut clocks,
                peripherals.sercom7,
                &mut peripherals.mclk,
                58.MHz(),
                &mut delay,
            )
            .unwrap();

        let config = EmbeddedBackendConfig {
            font_regular: FONT_10X20,
            ..Default::default()
        };
        let backend = EmbeddedBackend::new(&mut display, config);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_center(frame, "WIO SD EDITOR", "Starting..."))
            .ok();

        let top_right = pins.button1.into_pull_up_input();
        let top_middle = pins.button2.into_pull_up_input();
        let top_left = pins.button3.into_pull_up_input();
        let down = pins.switch_x.into_pull_up_input();
        let right = pins.switch_y.into_pull_up_input();
        let click = pins.switch_z.into_pull_up_input();
        let up = pins.switch_u.into_pull_up_input();
        let left = pins.switch_b.into_pull_up_input();

        let gclk0 = clocks.gclk0();

        // The 40-pin header's power rails are switched off until the MCU
        // enables them (3V3_OUTPUT_CTR on PC15 is active low, 5V_OUTPUT_CTR
        // on PC14 is active high). The battery chassis pulls its fuel-gauge
        // I2C lines up to the header's 3V3 rail, so with the rail dead those
        // resistors drag SDA/SCL low and every gauge read times out. The 5V
        // rail lets the chassis charge its battery from the terminal's USB
        // port, which is what the CHG indicator state reports.
        let mut header_3v3_enable = pins.output_ctr_3v3.into_push_pull_output();
        let _ = OutputPin::set_low(&mut header_3v3_enable);
        let mut header_5v_enable = pins.output_ctr_5v.into_push_pull_output();
        let _ = OutputPin::set_high(&mut header_5v_enable);

        let mut battery_i2c = BatteryI2c::new(pins.i2c1_sda.into(), pins.i2c1_scl.into());

        let sd_pins = wio::SDCard {
            cs: pins.sd_cs,
            mosi: pins.sd_mosi,
            sck: pins.sd_sck,
            miso: pins.sd_miso,
            det: pins.sd_det,
        };
        // Keep the SERCOM clock token alive for as long as its SPI peripheral.
        let _sd_clock = clocks.sercom6_core(&gclk0).unwrap();
        let pads = spi::Pads::default()
            .data_out(sd_pins.mosi)
            .data_in(sd_pins.miso)
            .sclk(sd_pins.sck);
        let sd_bus = spi::Config::new(
            &peripherals.mclk,
            peripherals.sercom6,
            pads,
            _sd_clock.freq(),
        )
        .spi_mode(spi::MODE_0)
        .baud(400.kHz())
        .enable();
        let sd_device = match SelectedSdDevice::new(sd_bus, sd_pins.cs.into_push_pull_output()) {
            Ok(device) => device,
            Err(()) => {
                // Without a working SD SPI bus there is nothing this firmware
                // can do; halt visibly rather than panicking into panic_halt
                // behind a frozen "Starting..." splash.
                terminal
                    .draw(|frame| draw_center(frame, "ERROR", "SD SPI init failed; power cycle"))
                    .ok();
                loop {
                    cortex_m::asm::wfi();
                }
            }
        };
        let sd_controller = Controller::new(sd_device, delay);
        let card_detect = sd_pins.det.into_pull_up_input();
        let controller = Rc::new(RefCell::new(sd_controller));

        let mut screen = Screen::Missing;
        let mut volume: Option<Fs> = None;
        let mut mounted_identity = None;
        let mut input = InputEngine::new();
        let mut last_poll = timer.count32();
        let mut last_battery_poll = last_poll.wrapping_sub(BATTERY_POLL_TICKS);
        let mut battery = None;
        let mut last_present = false;
        let mut observed_present = false;
        let mut present_changed_at = last_poll;
        terminal.draw(|frame| draw(frame, &screen, battery)).ok();
        let mut dirty = false;

        loop {
            let now = timer.count32();
            if now.wrapping_sub(last_poll) < POLL_TICKS {
                continue;
            }
            last_poll = now;

            if now.wrapping_sub(last_battery_poll) >= BATTERY_POLL_TICKS {
                last_battery_poll = now;
                let new_battery = read_battery(&mut battery_i2c);
                if new_battery != battery {
                    battery = new_battery;
                    dirty = true;
                }
            }

            let raw_present = card_detect.is_low().unwrap_or(false);
            if raw_present != observed_present {
                observed_present = raw_present;
                present_changed_at = now;
            }
            // Removal acts on the first absent sample; insertion waits for
            // the raw reading to hold steady for the settle window.
            let present = observed_present;
            if present != last_present
                && (!present || now.wrapping_sub(present_changed_at) >= CARD_SETTLE_TICKS)
            {
                last_present = present;
                dirty = true;
                if !present {
                    volume = None;
                    mounted_identity = None;
                    controller.borrow().mark_card_uninit();
                    screen = mark_card_removed(screen);
                } else {
                    let previous_screen = mem::replace(&mut screen, Screen::Mounting);
                    terminal.draw(|frame| draw(frame, &screen, battery)).ok();
                    screen = complete_mount(
                        previous_screen,
                        &controller,
                        &mut volume,
                        &mut mounted_identity,
                        now,
                    );
                }
            }

            // A dismissed Fatal screen or a discarded editor session can land
            // on Missing while an unmountable card stayed in the slot. Card
            // detection is edge-triggered, so retry the mount here; this
            // cannot loop because every mount failure leaves a non-Missing
            // screen (FormatPrompt or Fatal).
            if volume.is_none() && last_present && matches!(screen, Screen::Missing) {
                let previous_screen = mem::replace(&mut screen, Screen::Mounting);
                terminal.draw(|frame| draw(frame, &screen, battery)).ok();
                screen = complete_mount(
                    previous_screen,
                    &controller,
                    &mut volume,
                    &mut mounted_identity,
                    now,
                );
                dirty = true;
            }

            let raw = RawButtons::default()
                .with(Button::TopLeft, top_left.is_low().unwrap_or(false))
                .with(Button::TopMiddle, top_middle.is_low().unwrap_or(false))
                .with(Button::TopRight, top_right.is_low().unwrap_or(false))
                .with(Button::Up, up.is_low().unwrap_or(false))
                .with(Button::Left, left.is_low().unwrap_or(false))
                .with(Button::Click, click.is_low().unwrap_or(false))
                .with(Button::Right, right.is_low().unwrap_or(false))
                .with(Button::Down, down.is_low().unwrap_or(false));

            if let Some(button) = input.update(raw, now) {
                screen = match handle_button(screen, button, &mut volume, mounted_identity) {
                    HandleResult::Screen(next) => next,
                    // A pending rename/move runs synchronously; paint the
                    // busy notice before starting so the display does not
                    // freeze on the old screen.
                    HandleResult::Op(op) => {
                        terminal
                            .draw(|frame| draw_busy(frame, op.title(), battery))
                            .ok();
                        run_fs_op(op, &mut volume)
                    }
                };
                dirty = true;
            }

            if let Screen::FormatPrompt {
                cancelled: false,
                opened_at,
                ..
            } = &screen
            {
                let opened_at = *opened_at;
                let held = input.held_ticks(Button::TopRight, now);
                // The press must have started at or after the prompt appeared
                // (wrapping-safe); otherwise a button already held during
                // mounting would trigger formatting the instant the prompt
                // opened.
                if held >= FORMAT_HOLD_TICKS && now.wrapping_sub(opened_at) >= held {
                    screen = Screen::Formatting;
                    terminal.draw(|frame| draw(frame, &screen, battery)).ok();
                    screen = match format_card(&controller, now) {
                        Ok(()) => match mount_card(&controller) {
                            Ok((new_volume, identity)) => {
                                let mut explorer = Explorer::root();
                                explorer.refresh(&new_volume);
                                volume = Some(new_volume);
                                mounted_identity = Some(identity);
                                Screen::Explorer(explorer)
                            }
                            Err(MountFailure::Io) => {
                                Screen::Fatal("Formatting failed: SD I/O error")
                            }
                            // The formatter never writes the partition types
                            // that make mount_card report Unsupported, so any
                            // non-I/O mount failure means the fresh format did
                            // not read back as expected.
                            Err(_) => Screen::Fatal("Formatting failed verification"),
                        },
                        Err(FormatFailure::Io) => Screen::Fatal("Formatting failed: SD I/O error"),
                        Err(FormatFailure::TooSmall) => Screen::Fatal("Card too small for FAT32"),
                        Err(FormatFailure::TooLarge) => Screen::Fatal("Card too large to format"),
                        Err(FormatFailure::Failed) => {
                            Screen::Fatal("Formatting failed verification")
                        }
                    };
                    dirty = true;
                }
            }

            if dirty {
                terminal.draw(|frame| draw(frame, &screen, battery)).ok();
                dirty = false;
            }
        }
    }

    fn read_battery(i2c: &mut BatteryI2c) -> Option<BatteryStatus> {
        // A valid response from the chassis fuel gauge is the accessory
        // presence signal. BAT_DET reports cell insertion inside the chassis,
        // so using it here can hide a connected (and responsive) chassis.
        let percent = i2c.read_word(BQ27441_STATE_OF_CHARGE)?;
        if percent > 100 {
            return None;
        }

        // Current is secondary telemetry. Keep displaying the charge level if
        // this read is temporarily unavailable instead of hiding the chassis.
        let charging = i2c
            .read_word(BQ27441_AVERAGE_CURRENT)
            .is_some_and(|current| current as i16 > 0);
        Some(BatteryStatus {
            percent: percent as u8,
            charging,
        })
    }

    fn mount_card(
        controller: &Rc<RefCell<Controller>>,
    ) -> Result<(Fs, MediaIdentity), MountFailure> {
        {
            controller
                .borrow()
                .spi(|device| device.prepare_card())
                .map_err(|_| MountFailure::Io)?;
            controller.borrow().mark_card_uninit();
        }
        let device = ControllerDevice::new(controller.clone());
        let mut stream = SdStream::new(device).map_err(|_| MountFailure::Io)?;
        let sectors = (stream.len() / 512).min(u32::MAX as u64) as u32;
        controller.borrow().spi(|spi| {
            // Keep the BSP's SD-safe initialization rate for all I/O. Text
            // files are capped at 32 KiB, so reliability wins over throughput.
            spi.bus_mut()
                .reconfigure(|config| config.set_baud(400.kHz()));
        });
        let layout = probe_fat(&mut stream, sectors).map_err(|error| match error {
            ProbeError::Io(_) => MountFailure::Io,
            ProbeError::Unsupported => MountFailure::Unsupported,
            ProbeError::Invalid => MountFailure::Invalid,
        })?;
        // A FAT volume with no partition table ("superfloppy") passes the
        // probe but embedded-sdmmc only mounts MBR slots; treat it like any
        // other unmountable-but-recognized layout so the user is offered the
        // in-app format, which writes an MBR.
        if layout.start_lba == 0 {
            return Err(MountFailure::Unsupported);
        }
        let identity = MediaIdentity {
            sectors,
            partition_start: layout.start_lba,
            volume_serial: layout.volume_serial,
        };
        drop(stream);
        let volume = Fs::mount(ControllerDevice::new(controller.clone()), layout)
            .map_err(|error| match error {
                embedded_sdmmc::Error::DeviceError(_) => MountFailure::Io,
                embedded_sdmmc::Error::Unsupported => MountFailure::Unsupported,
                _ => MountFailure::Invalid,
            })?;
        Ok((volume, identity))
    }

    /// Mounts a freshly available card and derives the next screen from the
    /// one that was showing. The caller has already switched the display to
    /// `Screen::Mounting`.
    fn complete_mount(
        previous: Screen,
        controller: &Rc<RefCell<Controller>>,
        volume: &mut Option<Fs>,
        mounted_identity: &mut Option<MediaIdentity>,
        now: u32,
    ) -> Screen {
        match mount_card(controller) {
            Ok((new_volume, identity)) => {
                let mut screen = accept_inserted_card(previous, identity);
                if let Screen::Explorer(explorer) = &mut screen {
                    explorer.refresh(&new_volume);
                }
                *volume = Some(new_volume);
                *mounted_identity = Some(identity);
                screen
            }
            Err(failure) => {
                let mark = |editor: &mut Editor| {
                    if failure == MountFailure::Io {
                        // The card in the slot may still be the original; only
                        // its probe failed. Keep save gated on a clean
                        // identity-matching mount instead of asserting the
                        // card is different.
                        editor.media = EditorMedia::Removed;
                        editor.status = Some("SD read failed; remove and reinsert".into());
                    } else {
                        editor.media = EditorMedia::Different;
                    }
                };
                match previous {
                    Screen::Editor(mut editor) => {
                        mark(&mut editor);
                        Screen::Editor(editor)
                    }
                    Screen::ExitPrompt { mut editor, choice } => {
                        mark(&mut editor);
                        Screen::ExitPrompt { editor, choice }
                    }
                    _ => format_prompt(failure, now),
                }
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FormatFailure {
        Io,
        TooSmall,
        TooLarge,
        Failed,
    }

    /// `seed` perturbs the new volume serial. The caller passes the RTC tick
    /// at the moment the user confirmed the format: uptime plus human timing
    /// is the only entropy available on a device without a calendar clock.
    fn format_card(controller: &Rc<RefCell<Controller>>, seed: u32) -> Result<(), FormatFailure> {
        let device = ControllerDevice::new(controller.clone());
        let mut stream = SdStream::new(device).map_err(|_| FormatFailure::Io)?;
        let total_sectors = stream.len() / 512;
        // MBR LBA fields and the backup-GPT erase cannot address sectors
        // past 2 TiB; refuse rather than write a wrong geometry or leave a
        // stale backup GPT header at the true last LBA.
        if total_sectors > u64::from(u32::MAX) {
            return Err(FormatFailure::TooLarge);
        }
        let sectors = total_sectors as u32;
        format_fat32(&mut stream, sectors, seed).map_err(|error| match error {
            wio_terminal_sd_editor::FormatError::Io(_) => FormatFailure::Io,
            wio_terminal_sd_editor::FormatError::TooSmall => FormatFailure::TooSmall,
            _ => FormatFailure::Failed,
        })?;
        Ok(())
    }

    fn format_prompt(failure: MountFailure, now: u32) -> Screen {
        if failure == MountFailure::Io {
            return Screen::Fatal("SD read failed; remove and reinsert");
        }
        Screen::FormatPrompt {
            reason: match failure {
                MountFailure::Unsupported => "Unsupported filesystem",
                MountFailure::Invalid => "Invalid or damaged filesystem",
                MountFailure::Io => unreachable!(),
            },
            cancelled: false,
            opened_at: now,
        }
    }

    fn mark_card_removed(screen: Screen) -> Screen {
        match screen {
            Screen::Editor(mut editor) => {
                editor.media = EditorMedia::Removed;
                Screen::Editor(editor)
            }
            Screen::ExitPrompt { mut editor, choice } => {
                editor.media = EditorMedia::Removed;
                Screen::ExitPrompt { editor, choice }
            }
            _ => Screen::Missing,
        }
    }

    fn accept_inserted_card(screen: Screen, identity: MediaIdentity) -> Screen {
        match screen {
            Screen::Editor(mut editor) => {
                // Any status ("Reinsert the original SD card to save", ...)
                // described the previous media state.
                editor.status = None;
                if editor.identity == identity {
                    editor.media = EditorMedia::Ready;
                } else {
                    editor.media = EditorMedia::Different;
                    editor.explorer = Explorer::root();
                }
                Screen::Editor(editor)
            }
            Screen::ExitPrompt { mut editor, choice } => {
                editor.status = None;
                if editor.identity == identity {
                    editor.media = EditorMedia::Ready;
                } else {
                    editor.media = EditorMedia::Different;
                    editor.explorer = Explorer::root();
                }
                Screen::ExitPrompt { editor, choice }
            }
            _ => Screen::Explorer(Explorer::root()),
        }
    }

    fn handle_button(
        screen: Screen,
        button: Button,
        volume: &mut Option<Fs>,
        identity: Option<MediaIdentity>,
    ) -> HandleResult {
        match screen {
            Screen::Explorer(explorer) => {
                handle_explorer(explorer, button, volume, identity).into()
            }
            Screen::NewMenu(menu) => handle_new_menu(menu, button).into(),
            Screen::ActionMenu(menu) => handle_action_menu(menu, button, volume).into(),
            Screen::Naming(entry) => handle_name_entry(entry, button, volume, identity),
            Screen::MovePicker(picker) => handle_move_picker(picker, button, volume),
            Screen::DeletePrompt {
                explorer,
                item,
                choice,
            } => handle_delete_prompt(explorer, item, choice, button),
            Screen::Editor(editor) => handle_editor(editor, button, volume).into(),
            Screen::ExitPrompt { editor, choice } => {
                handle_exit(editor, choice, button, volume).into()
            }
            Screen::FormatPrompt {
                reason, opened_at, ..
            } if button == Button::TopLeft => Screen::FormatPrompt {
                reason,
                cancelled: true,
                opened_at,
            }
            .into(),
            // A retry needs no fresh opened_at: it fires on a new press event,
            // so that press necessarily began after the prompt opened.
            Screen::FormatPrompt {
                reason,
                cancelled: true,
                opened_at,
            } if button == Button::TopRight => Screen::FormatPrompt {
                reason,
                cancelled: false,
                opened_at,
            }
            .into(),
            Screen::Fatal(_) if button == Button::TopRight => Screen::Missing.into(),
            other => other.into(),
        }
    }

    /// Land a finished rename/move on the destination entry. `source_remains`
    /// marks success-with-caveat: the destination holds a verified copy and
    /// only the source entry's removal failed (`DeleteIncomplete` or
    /// `SourceRemains`), so reporting "<verb> failed" would invite the
    /// user to delete the only good copy.
    fn complete_move_op(
        card: &Fs,
        mut explorer: Explorer,
        destination: &str,
        verb: &str,
        source_remains: bool,
    ) -> Screen {
        explorer.refresh_select_path(card, destination);
        explorer.status = Some(if source_remains {
            format!("{verb}; {OLD_ENTRY_REMAINS}")
        } else {
            verb.into()
        });
        Screen::Explorer(explorer)
    }

    /// How a finished rename/move landed. Both `run_fs_op` arms classify
    /// their shared `move_entry_verified` result through [`move_outcome`]
    /// so the two operations can never drift apart on the same on-card
    /// outcome.
    enum MoveOutcome {
        /// The destination holds the verified entry; `source_remains`
        /// carries the success-with-caveat flag for [`complete_move_op`].
        Done { source_remains: bool },
        /// The operation did not land; show this status on the input
        /// screen.
        Rejected(String),
    }

    fn move_outcome(
        result: Result<(), FsOpError<embedded_sdmmc::SdCardError>>,
        fail_verb: &str,
        exists_status: &str,
    ) -> MoveOutcome {
        match result {
            Ok(()) => MoveOutcome::Done {
                source_remains: false,
            },
            Err(FsOpError::DeleteIncomplete(_) | FsOpError::SourceRemains(_)) => {
                MoveOutcome::Done {
                    source_remains: true,
                }
            }
            Err(FsOpError::AlreadyExists) => MoveOutcome::Rejected(exists_status.into()),
            Err(error) => MoveOutcome::Rejected(format!(
                "{fail_verb} failed: {}",
                save_failure_reason(&error)
            )),
        }
    }

    /// Run the rename/move a [`HandleResult::Op`] carries. The event loop
    /// calls this right after painting the busy notice.
    fn run_fs_op(op: PendingFsOp, volume: &mut Option<Fs>) -> Screen {
        let Some(card) = volume.as_ref() else {
            return Screen::Missing;
        };
        match op {
            PendingFsOp::Rename { mut entry, destination } => {
                let source = entry.source.as_ref().expect("rename has a source");
                match move_outcome(
                    card.move_entry_verified(&source.path, &destination),
                    "Rename",
                    "That name already exists",
                ) {
                    MoveOutcome::Done { source_remains } => {
                        complete_move_op(card, entry.explorer, &destination, "Renamed", source_remains)
                    }
                    MoveOutcome::Rejected(status) => {
                        entry.status = Some(status);
                        Screen::Naming(entry)
                    }
                }
            }
            PendingFsOp::Move { mut picker, destination } => {
                match move_outcome(
                    card.move_entry_verified(&picker.source.path, &destination),
                    "Move",
                    "That name already exists here",
                ) {
                    MoveOutcome::Done { source_remains } => {
                        let explorer = Explorer {
                            path: picker.path,
                            ..Explorer::root()
                        };
                        complete_move_op(card, explorer, &destination, "Moved", source_remains)
                    }
                    MoveOutcome::Rejected(status) => {
                        picker.status = Some(status);
                        Screen::MovePicker(picker)
                    }
                }
            }
            PendingFsOp::Delete { mut explorer, item } => {
                match card.delete_verified(&item.path, item.is_dir) {
                    Ok(()) => {
                        explorer.refresh(card);
                        explorer.status = Some(if item.is_dir {
                            "Folder deleted".into()
                        } else {
                            "File deleted".into()
                        });
                    }
                    Err(error) => {
                        explorer.refresh(card);
                        explorer.status = Some(match &error {
                            // Destruction persisted (contents reclaimed or
                            // children removed) before the failure:
                            // "incomplete" is the truth ("Delete
                            // incomplete: data removed" is 31 columns).
                            FsOpError::DeleteIncomplete(_) => {
                                format!("Delete incomplete: {}", save_failure_reason(&error))
                            }
                            // Refusals (read-only, non-empty folder, not
                            // found...) destroyed nothing; saying
                            // "incomplete" would claim partial destruction
                            // that never happened. "Not deleted: " plus the
                            // longest common reason ("folder is not empty")
                            // is exactly 32 columns; rarer long reasons
                            // clip via truncate.
                            _ => format!("Not deleted: {}", save_failure_reason(&error)),
                        });
                    }
                }
                Screen::Explorer(explorer)
            }
        }
    }

    fn handle_explorer(
        mut explorer: Explorer,
        button: Button,
        volume: &mut Option<Fs>,
        identity: Option<MediaIdentity>,
    ) -> Screen {
        let Some(card) = volume.as_ref() else {
            return Screen::Missing;
        };
        match button {
            Button::Up => {
                if explorer.selected > 0 {
                    explorer.selected -= 1;
                } else if explorer.offset > 0 {
                    explorer.offset -= 1;
                    explorer.refresh(card);
                }
            }
            Button::Down => {
                if explorer.selected + 1 < explorer.entries.len() {
                    explorer.selected += 1;
                } else if explorer.offset + explorer.entries.len() < explorer.total {
                    explorer.offset += 1;
                    explorer.refresh(card);
                    explorer.selected = explorer.entries.len().saturating_sub(1);
                }
            }
            Button::Left | Button::TopLeft => explorer.go_parent(card),
            Button::TopMiddle => {
                return Screen::NewMenu(NewMenu {
                    explorer,
                    choice: NewChoice::TextFile,
                });
            }
            Button::TopRight => {
                let item = explorer.entries.get(explorer.selected).cloned();
                return Screen::ActionMenu(ActionMenu {
                    explorer,
                    item,
                    choice: ActionChoice::Rename,
                });
            }
            Button::Right | Button::Click => {
                let Some(item) = explorer.entries.get(explorer.selected).cloned() else {
                    return Screen::Explorer(explorer);
                };
                if item.is_dir {
                    explorer.path = item.path;
                    explorer.offset = 0;
                    explorer.selected = 0;
                    explorer.refresh(card);
                } else if !is_txt_file(&item.name) {
                    explorer.status = Some("Only .txt files can be edited".into());
                } else {
                    match card.load_text(&item.path) {
                        Ok(buffer) => {
                            return Screen::Editor(Editor {
                                explorer,
                                path: item.path,
                                name: item.name,
                                buffer,
                                keyboard: Keyboard::new(),
                                keyboard_visible: true,
                                scroll_line: 0,
                                horizontal: 0,
                                status: None,
                                media: EditorMedia::Ready,
                                identity: identity.expect("mounted explorer has identity"),
                            });
                        }
                        Err(wio_terminal_sd_editor::LoadError::TooLarge) => {
                            explorer.status = Some("File exceeds the 32 KiB editor limit".into());
                        }
                        Err(wio_terminal_sd_editor::LoadError::InvalidUtf8) => {
                            explorer.status = Some("File is not valid UTF-8 text".into());
                        }
                        Err(_) => explorer.status = Some("Could not open the file".into()),
                    }
                }
            }
        }
        Screen::Explorer(explorer)
    }

    fn handle_new_menu(mut menu: NewMenu, button: Button) -> Screen {
        match button {
            Button::TopLeft => Screen::Explorer(menu.explorer),
            Button::Up | Button::Down | Button::Left | Button::Right => {
                menu.choice = menu.choice.next();
                Screen::NewMenu(menu)
            }
            Button::Click | Button::TopMiddle | Button::TopRight => {
                let mode = match menu.choice {
                    NewChoice::TextFile => NameMode::NewText,
                    NewChoice::Folder => NameMode::NewFolder,
                };
                Screen::Naming(NameEntry {
                    explorer: menu.explorer,
                    mode,
                    source: None,
                    name: String::new(),
                    cursor: 0,
                    keyboard: Keyboard::new(),
                    keyboard_visible: true,
                    status: None,
                })
            }
        }
    }

    fn handle_action_menu(mut menu: ActionMenu, button: Button, volume: &mut Option<Fs>) -> Screen {
        match button {
            Button::TopLeft => return Screen::Explorer(menu.explorer),
            Button::Up | Button::Left => menu.choice = menu.choice.previous(),
            Button::Down | Button::Right => menu.choice = menu.choice.next(),
            Button::Click | Button::TopMiddle | Button::TopRight => match menu.choice {
                ActionChoice::Refresh => {
                    if let Some(card) = volume.as_ref() {
                        menu.explorer.refresh(card);
                    }
                    return Screen::Explorer(menu.explorer);
                }
                ActionChoice::Rename => {
                    let Some(item) = menu.item else {
                        menu.explorer.status = Some("No item selected".into());
                        return Screen::Explorer(menu.explorer);
                    };
                    // Prefill with the stored 8.3 name (the last path
                    // component), not the display name: a long display name
                    // would fail 8.3 validation before the user typed a key.
                    let name = String::from(leaf_name(&item.path));
                    return Screen::Naming(NameEntry {
                        explorer: menu.explorer,
                        mode: NameMode::Rename,
                        source: Some(item),
                        cursor: name.len(),
                        name,
                        keyboard: Keyboard::new(),
                        keyboard_visible: true,
                        status: None,
                    });
                }
                ActionChoice::Move => {
                    let Some(item) = menu.item else {
                        menu.explorer.status = Some("No item selected".into());
                        return Screen::Explorer(menu.explorer);
                    };
                    let mut picker = MovePicker::new(menu.explorer, item);
                    if let Some(card) = volume.as_ref() {
                        picker.refresh(card);
                    }
                    return Screen::MovePicker(picker);
                }
                ActionChoice::Delete => {
                    let Some(item) = menu.item else {
                        menu.explorer.status = Some("No item selected".into());
                        return Screen::Explorer(menu.explorer);
                    };
                    return Screen::DeletePrompt {
                        explorer: menu.explorer,
                        item,
                        choice: DeleteChoice::Cancel,
                    };
                }
            },
        }
        Screen::ActionMenu(menu)
    }

    fn handle_name_entry(
        mut entry: NameEntry,
        button: Button,
        volume: &mut Option<Fs>,
        identity: Option<MediaIdentity>,
    ) -> HandleResult {
        if button == Button::TopLeft {
            // Re-list before handing the explorer back: a failed rename may
            // have left an on-card ghost (partial copy) that the listing
            // captured before the operation cannot show.
            let Some(card) = volume.as_ref() else {
                return Screen::Missing.into();
            };
            entry.explorer.refresh(card);
            return Screen::Explorer(entry.explorer).into();
        }
        if button == Button::TopMiddle {
            entry.keyboard_visible = !entry.keyboard_visible;
            return Screen::Naming(entry).into();
        }
        if button == Button::TopRight {
            let validation = match entry.mode {
                NameMode::NewText => validate_file_stem(&entry.name),
                NameMode::NewFolder | NameMode::Rename => validate_entry_name(&entry.name),
            };
            if let Err(message) = validation {
                entry.status = Some(message.into());
                return Screen::Naming(entry).into();
            }
            let Some(card) = volume.as_ref() else {
                return Screen::Missing.into();
            };
            match entry.mode {
                NameMode::NewText => {
                    let name = format!("{}.txt", entry.name);
                    let path = join_path(&entry.explorer.path, &name);
                    match card.create_empty(&path) {
                        Ok(()) => {
                            return Screen::Editor(Editor {
                                explorer: entry.explorer,
                                path,
                                name,
                                buffer: TextBuffer::empty(),
                                keyboard: Keyboard::new(),
                                keyboard_visible: true,
                                scroll_line: 0,
                                horizontal: 0,
                                status: None,
                                media: EditorMedia::Ready,
                                identity: identity.expect("mounted name entry has identity"),
                            })
                            .into();
                        }
                        Err(FsOpError::AlreadyExists) => {
                            entry.status = Some("That file already exists".into())
                        }
                        Err(error) => {
                            entry.status =
                                Some(format!("Create failed: {}", save_failure_reason(&error)))
                        }
                    }
                }
                NameMode::NewFolder => {
                    let path = join_path(&entry.explorer.path, &entry.name);
                    match card.create_directory_verified(&path) {
                        Ok(()) => {
                            entry.explorer.refresh_select_path(card, &path);
                            entry.explorer.status = Some("Folder created".into());
                            return Screen::Explorer(entry.explorer).into();
                        }
                        Err(FsOpError::AlreadyExists) => {
                            entry.status = Some("That name already exists".into())
                        }
                        Err(error) => {
                            entry.status =
                                Some(format!("Create failed: {}", save_failure_reason(&error)))
                        }
                    }
                }
                NameMode::Rename => {
                    // The copy-based rename is slow; hand it to the event
                    // loop so a busy notice is painted before it starts.
                    let destination = join_path(&entry.explorer.path, &entry.name);
                    return HandleResult::Op(PendingFsOp::Rename { entry, destination });
                }
            }
            return Screen::Naming(entry).into();
        }

        if !entry.keyboard_visible {
            match button {
                Button::Left => entry.cursor = previous_char_boundary(&entry.name, entry.cursor),
                Button::Right => entry.cursor = next_char_boundary(&entry.name, entry.cursor),
                Button::Click => entry.keyboard_visible = true,
                _ => {}
            }
            return Screen::Naming(entry).into();
        }
        if move_keyboard(&mut entry.keyboard, button) {
            return Screen::Naming(entry).into();
        }
        if button == Button::Click {
            let key = entry.keyboard.selected();
            match key {
                Key::Character(ch) if entry.can_insert(ch) => {
                    entry.name.insert(entry.cursor, ch);
                    entry.cursor += ch.len_utf8();
                    entry.status = None;
                }
                Key::Space if entry.can_insert(' ') => {
                    entry.name.insert(entry.cursor, ' ');
                    entry.cursor += 1;
                    entry.status = None;
                }
                Key::Backspace if entry.cursor > 0 => {
                    entry.cursor = previous_char_boundary(&entry.name, entry.cursor);
                    entry.name.remove(entry.cursor);
                    entry.status = None;
                }
                Key::Delete if entry.cursor < entry.name.len() => {
                    entry.name.remove(entry.cursor);
                    entry.status = None;
                }
                Key::Enter => entry.status = Some("Use DONE to finish the name".into()),
                Key::Case | Key::Page => entry.keyboard.activate_meta(key),
                _ => {}
            }
        }
        Screen::Naming(entry).into()
    }

    fn handle_move_picker(
        mut picker: MovePicker,
        button: Button,
        volume: &mut Option<Fs>,
    ) -> HandleResult {
        let Some(card) = volume.as_ref() else {
            return Screen::Missing.into();
        };
        match button {
            Button::TopLeft => {
                // Re-list before handing the origin explorer back: a failed
                // move may have changed the card since it was captured.
                let mut origin = picker.origin;
                origin.refresh(card);
                return Screen::Explorer(origin).into();
            }
            Button::Up => {
                if picker.selected > 0 {
                    picker.selected -= 1;
                } else if picker.offset > 0 {
                    picker.offset -= 1;
                    picker.refresh(card);
                }
            }
            Button::Down => {
                if picker.selected + 1 < picker.entries.len() {
                    picker.selected += 1;
                } else if picker.offset + picker.entries.len() < picker.total {
                    picker.offset += 1;
                    picker.refresh(card);
                    picker.selected = picker.entries.len().saturating_sub(1);
                }
            }
            Button::Left => picker.go_parent(card),
            Button::Right | Button::Click => {
                if let Some(item) = picker.entries.get(picker.selected) {
                    picker.path = item.path.clone();
                    picker.offset = 0;
                    picker.selected = 0;
                    picker.refresh(card);
                }
            }
            Button::TopRight => picker.refresh(card),
            Button::TopMiddle => {
                let old_parent = parent_path(&picker.source.path);
                if old_parent.eq_ignore_ascii_case(&picker.path) {
                    picker.status = Some("Item is already in this folder".into());
                    return Screen::MovePicker(picker).into();
                }
                // The destination keeps the entry's stored 8.3 name (the last
                // path component), not the display name: a long display name
                // cannot be created on the destination side.
                let destination = join_path(&picker.path, leaf_name(&picker.source.path));
                // The copy-based move is slow; hand it to the event loop so
                // a busy notice is painted before it starts.
                return HandleResult::Op(PendingFsOp::Move {
                    picker,
                    destination,
                });
            }
        }
        Screen::MovePicker(picker).into()
    }

    fn handle_delete_prompt(
        explorer: Explorer,
        item: DirectoryItem,
        mut choice: DeleteChoice,
        button: Button,
    ) -> HandleResult {
        match button {
            Button::Left | Button::Right | Button::Up | Button::Down => {
                choice = match choice {
                    DeleteChoice::Cancel => DeleteChoice::Delete,
                    DeleteChoice::Delete => DeleteChoice::Cancel,
                }
            }
            Button::TopLeft => return Screen::Explorer(explorer).into(),
            Button::Click | Button::TopMiddle | Button::TopRight => match choice {
                DeleteChoice::Cancel => return Screen::Explorer(explorer).into(),
                // A recursive folder delete walks and rewrites the card for
                // seconds; hand it to the event loop so the busy notice
                // paints first (run_fs_op handles a missing card).
                DeleteChoice::Delete => {
                    return HandleResult::Op(PendingFsOp::Delete { explorer, item });
                }
            },
        }
        Screen::DeletePrompt {
            explorer,
            item,
            choice,
        }
        .into()
    }

    fn handle_editor(mut editor: Editor, button: Button, volume: &mut Option<Fs>) -> Screen {
        match button {
            Button::TopLeft => {
                // The dialog repeats editor.status as save feedback, so a
                // message left over from earlier editing must not carry in.
                editor.status = None;
                return Screen::ExitPrompt {
                    editor,
                    choice: ExitChoice::Cancel,
                };
            }
            Button::TopMiddle => editor.keyboard_visible = !editor.keyboard_visible,
            Button::TopRight => {
                save_editor(&mut editor, volume);
            }
            _ if editor.keyboard_visible => {
                if !move_keyboard(&mut editor.keyboard, button) && button == Button::Click {
                    let key = editor.keyboard.selected();
                    let result = match key {
                        Key::Character(ch) => editor.buffer.insert_char(ch),
                        Key::Space => editor.buffer.insert_char(' '),
                        Key::Backspace => {
                            editor.buffer.backspace();
                            Ok(())
                        }
                        Key::Delete => {
                            editor.buffer.delete();
                            Ok(())
                        }
                        Key::Enter => editor.buffer.insert_newline(),
                        Key::Case | Key::Page => {
                            editor.keyboard.activate_meta(key);
                            Ok(())
                        }
                    };
                    if result == Err(EditError::TooLarge) {
                        editor.status = Some("32 KiB document limit reached".into());
                    } else {
                        editor.status = None;
                    }
                }
            }
            Button::Up => editor.buffer.move_up(),
            Button::Down => editor.buffer.move_down(),
            Button::Left => editor.buffer.move_left(),
            Button::Right => editor.buffer.move_right(),
            Button::Click => editor.keyboard_visible = true,
        }
        editor.ensure_cursor_visible();
        Screen::Editor(editor)
    }

    /// Returns whether the document verifiably reached the card.
    fn save_editor(editor: &mut Editor, volume: &mut Option<Fs>) -> bool {
        if editor.media != EditorMedia::Ready {
            editor.status = Some(
                match editor.media {
                    EditorMedia::Removed => "Reinsert the original SD card to save",
                    EditorMedia::Different => "Different SD card: save is disabled",
                    EditorMedia::Ready => unreachable!(),
                }
                .into(),
            );
            return false;
        }
        let Some(card) = volume.as_ref() else {
            editor.status = Some("SD card is unavailable".into());
            return false;
        };
        match card.save_transactional(&editor.path, editor.buffer.bytes()) {
            Ok(()) => {
                editor.buffer.mark_clean();
                editor.status = Some("Saved".into());
                true
            }
            Err(error) => {
                editor.status = Some(match &error.backup_kept {
                    // "Save failed; kept ~WIO0000.BAK" is 30 columns: it
                    // fits the editor's 32-column status line and the exit
                    // prompt's 30-column repeat of it. The surviving backup
                    // is the actionable fact for a save whose target may be
                    // partial.
                    Some(backup) => format!("Save failed; kept {}", leaf_name(backup)),
                    None => format!("Save failed: {}", save_failure_reason(&error.error)),
                });
                false
            }
        }
    }

    fn handle_exit(
        mut editor: Editor,
        mut choice: ExitChoice,
        button: Button,
        volume: &mut Option<Fs>,
    ) -> Screen {
        match button {
            Button::Left | Button::Up => choice = choice.left(),
            Button::Right | Button::Down => choice = choice.right(),
            Button::TopLeft => return Screen::Editor(editor),
            Button::Click => match choice {
                ExitChoice::Cancel => return Screen::Editor(editor),
                ExitChoice::Discard => {
                    if let Some(card) = volume.as_ref() {
                        editor.explorer.refresh(card);
                        return Screen::Explorer(editor.explorer);
                    }
                    return Screen::Missing;
                }
                ExitChoice::Save => {
                    // Exit only on a verified save. Dirtiness is not a success
                    // proxy: a failed save of a never-modified buffer would
                    // otherwise exit silently.
                    if !save_editor(&mut editor, volume) {
                        return Screen::ExitPrompt { editor, choice };
                    }
                    if let Some(card) = volume.as_ref() {
                        editor.explorer.refresh(card);
                    }
                    return Screen::Explorer(editor.explorer);
                }
            },
            _ => {}
        }
        Screen::ExitPrompt { editor, choice }
    }

    fn move_keyboard(keyboard: &mut Keyboard, button: Button) -> bool {
        match button {
            Button::Up => keyboard.move_up(),
            Button::Down => keyboard.move_down(),
            Button::Left => keyboard.move_left(),
            Button::Right => keyboard.move_right(),
            _ => return false,
        }
        true
    }

    fn previous_char_boundary(text: &str, at: usize) -> usize {
        text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next_char_boundary(text: &str, at: usize) -> usize {
        text[at..]
            .char_indices()
            .nth(1)
            .map_or(text.len(), |(i, _)| at + i)
    }

    /// Shown while an operation holds the card busy (formatting, or a
    /// pending rename/move).
    const CARD_BUSY_WARNING: &str = "Do not remove the card";

    /// The shell every full repaint shares: black clear, content, and the
    /// battery chassis painted last so it overlays whatever the content
    /// drew. `content` receives whether the battery indicator is shown.
    fn draw_frame(
        frame: &mut Frame,
        battery: Option<BatteryStatus>,
        content: impl FnOnce(&mut Frame, bool),
    ) {
        let area = frame.area();
        frame.render_widget(UiBlock::new().style(Style::new().bg(Color::Black)), area);
        content(frame, battery.is_some());
        if let Some(battery) = battery {
            draw_battery(frame, battery);
        }
    }

    fn draw(frame: &mut Frame, screen: &Screen, battery: Option<BatteryStatus>) {
        draw_frame(frame, battery, |frame, battery_shown| match screen {
            Screen::Missing => draw_center(frame, "SD CARD", "Insert an SD card"),
            Screen::Mounting => draw_center(frame, "SD CARD", "Mounting..."),
            Screen::Formatting => draw_center(frame, "FORMATTING", CARD_BUSY_WARNING),
            Screen::Fatal(message) => draw_center(frame, "ERROR", message),
            Screen::FormatPrompt {
                reason, cancelled, ..
            } => draw_format_prompt(frame, reason, *cancelled),
            Screen::Explorer(explorer) => draw_explorer(frame, explorer, battery_shown),
            Screen::NewMenu(menu) => draw_new_menu(frame, menu, battery_shown),
            Screen::ActionMenu(menu) => draw_action_menu(frame, menu, battery_shown),
            Screen::Naming(entry) => draw_name_entry(frame, entry, battery_shown),
            Screen::MovePicker(picker) => draw_move_picker(frame, picker, battery_shown),
            Screen::DeletePrompt { item, choice, .. } => {
                draw_delete_prompt(frame, item, *choice, battery_shown)
            }
            Screen::Editor(editor) => draw_editor(frame, editor, true, battery_shown),
            Screen::ExitPrompt { editor, choice } => {
                // The dialog shows the status itself; suppress the editor's
                // own copy so the message renders exactly once.
                draw_editor(frame, editor, false, battery_shown);
                draw_exit_prompt(frame, *choice, editor.status.as_deref());
            }
        });
    }

    /// The blocking notice for a [`PendingFsOp`], painted by the event loop
    /// right before the operation runs; pending work is not a [`Screen`],
    /// so it cannot go through `draw`.
    fn draw_busy(frame: &mut Frame, title: &str, battery: Option<BatteryStatus>) {
        draw_frame(frame, battery, |frame, _| {
            draw_center(frame, title, CARD_BUSY_WARNING);
        });
    }

    fn draw_battery(frame: &mut Frame, battery: BatteryStatus) {
        let label = if battery.charging {
            format!("{}% CHG", battery.percent)
        } else {
            format!("{}% BAT", battery.percent)
        };
        frame.render_widget(
            Paragraph::new(label).alignment(Alignment::Right).style(
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(24, 0, 8, 1),
        );
    }

    fn draw_center(frame: &mut Frame, title: &str, message: &str) {
        frame.render_widget(
            Paragraph::new(title)
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Rect::new(0, 3, 32, 1),
        );
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            Rect::new(0, 5, 32, 2),
        );
    }

    fn draw_format_prompt(frame: &mut Frame, reason: &str, cancelled: bool) {
        draw_center(frame, "FORMAT SD CARD?", reason);
        frame.render_widget(
            Paragraph::new("Formatting erases ALL data")
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Rect::new(0, 7, 32, 1),
        );
        let footer = if cancelled {
            "Cancelled     RIGHT: retry"
        } else {
            "LEFT: cancel  Hold RIGHT 2s"
        };
        frame.render_widget(
            Paragraph::new(footer).alignment(Alignment::Center),
            Rect::new(0, 10, 32, 1),
        );
    }

    fn draw_explorer(frame: &mut Frame, explorer: &Explorer, battery_shown: bool) {
        header(frame, "SD FILES", "BROWSE", battery_shown);
        frame.render_widget(
            Paragraph::new(truncate(&explorer.path, 32)).style(Style::new().fg(Color::DarkGray)),
            Rect::new(0, 1, 32, 1),
        );
        for (row, item) in explorer.entries.iter().enumerate() {
            let marker = if item.is_dir { ">" } else { " " };
            let flags = if item.hidden || item.system { "*" } else { " " };
            let size = if item.is_dir {
                String::new()
            } else {
                compact_size(item.size)
            };
            let text = format!("{marker}{flags}{:<22}{:>6}", truncate(&item.name, 22), size);
            let style = if row == explorer.selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if item.is_dir {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::White)
            };
            frame.render_widget(
                Paragraph::new(text).style(style),
                Rect::new(0, 2 + row as u16, 32, 1),
            );
        }
        if explorer.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("(empty folder)").alignment(Alignment::Center),
                Rect::new(0, 5, 32, 1),
            );
        }
        if let Some(status) = &explorer.status {
            frame.render_widget(
                Paragraph::new(truncate(status, 32)).style(Style::new().fg(Color::Red)),
                Rect::new(0, 10, 32, 1),
            );
        } else {
            if let Some(free) = explorer.free_bytes {
                frame.render_widget(
                    Paragraph::new(format!("{} free", compact_size(free)))
                        .style(Style::new().fg(Color::DarkGray)),
                    Rect::new(0, 10, 16, 1),
                );
            }
            let position = if explorer.total == 0 {
                "0/0".into()
            } else {
                format!(
                    "{}/{}",
                    explorer.offset + explorer.selected + 1,
                    explorer.total
                )
            };
            frame.render_widget(
                Paragraph::new(position).alignment(Alignment::Right),
                // 16 cells: wide enough that even "99999/99999" renders
                // without clipping digits.
                Rect::new(16, 10, 16, 1),
            );
        }
        footer(frame, "UP", "NEW", "ACTIONS");
    }

    fn draw_new_menu(frame: &mut Frame, menu: &NewMenu, battery_shown: bool) {
        header(frame, "NEW", "SELECT", battery_shown);
        draw_menu_item(
            frame,
            3,
            "Text file (.txt)",
            matches!(menu.choice, NewChoice::TextFile),
        );
        draw_menu_item(frame, 5, "Folder", matches!(menu.choice, NewChoice::Folder));
        footer(frame, "CANCEL", "SELECT", "SELECT");
    }

    fn draw_action_menu(frame: &mut Frame, menu: &ActionMenu, battery_shown: bool) {
        header(frame, "ACTIONS", "SELECT", battery_shown);
        let name = menu
            .item
            .as_ref()
            .map_or("(no item)", |item| item.name.as_str());
        frame.render_widget(
            Paragraph::new(truncate(name, 32)).style(Style::new().fg(Color::DarkGray)),
            Rect::new(0, 1, 32, 1),
        );
        for (row, (label, selected)) in [
            ("Rename", matches!(menu.choice, ActionChoice::Rename)),
            ("Move", matches!(menu.choice, ActionChoice::Move)),
            ("Delete", matches!(menu.choice, ActionChoice::Delete)),
            ("Refresh", matches!(menu.choice, ActionChoice::Refresh)),
        ]
        .into_iter()
        .enumerate()
        {
            draw_menu_item(frame, 3 + row as u16 * 2, label, selected);
        }
        footer(frame, "CANCEL", "SELECT", "SELECT");
    }

    fn draw_menu_item(frame: &mut Frame, y: u16, label: &str, selected: bool) {
        let style = if selected {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::new().fg(Color::White)
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(style),
            Rect::new(2, y, 28, 1),
        );
    }

    fn draw_name_entry(frame: &mut Frame, entry: &NameEntry, battery_shown: bool) {
        let (title, suffix, action) = match entry.mode {
            NameMode::NewText => ("NEW TEXT FILE", ".txt", "CREATE"),
            NameMode::NewFolder => ("NEW FOLDER", "", "CREATE"),
            NameMode::Rename => ("RENAME", "", "DONE"),
        };
        header(frame, title, suffix, battery_shown);
        frame.render_widget(Paragraph::new("Name:"), Rect::new(0, 2, 32, 1));
        let cursor_chars = entry.name[..entry.cursor].chars().count();
        let start = cursor_chars.saturating_sub(28);
        let mut with_cursor = entry.name.clone();
        with_cursor.insert(entry.cursor, '|');
        let shown: String = with_cursor.chars().skip(start).take(31).collect();
        frame.render_widget(
            Paragraph::new(shown).style(Style::new().fg(Color::Cyan)),
            Rect::new(0, 3, 32, 1),
        );
        if let Some(status) = &entry.status {
            frame.render_widget(
                Paragraph::new(truncate(status, 32)).style(Style::new().fg(Color::Red)),
                Rect::new(0, 5, 32, 1),
            );
        }
        if entry.keyboard_visible {
            draw_keyboard(frame, &entry.keyboard, 6);
        } else {
            frame.render_widget(
                Paragraph::new("Keyboard hidden; CLICK to show").alignment(Alignment::Center),
                Rect::new(0, 7, 32, 1),
            );
        }
        footer(frame, "CANCEL", "KEYBOARD", action);
    }

    fn draw_move_picker(frame: &mut Frame, picker: &MovePicker, battery_shown: bool) {
        header(frame, "MOVE", "FOLDERS", battery_shown);
        frame.render_widget(
            Paragraph::new(truncate(&picker.path, 32)).style(Style::new().fg(Color::DarkGray)),
            Rect::new(0, 1, 32, 1),
        );
        for (row, item) in picker.entries.iter().enumerate() {
            let style = if row == picker.selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::new().fg(Color::Yellow)
            };
            frame.render_widget(
                Paragraph::new(format!("> {}", truncate(&item.name, 28))).style(style),
                Rect::new(0, 2 + row as u16, 32, 1),
            );
        }
        if picker.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("(no subfolders)").alignment(Alignment::Center),
                Rect::new(0, 5, 32, 1),
            );
        }
        if let Some(status) = &picker.status {
            frame.render_widget(
                Paragraph::new(truncate(status, 32)).style(Style::new().fg(Color::Red)),
                Rect::new(0, 10, 32, 1),
            );
        }
        footer(frame, "CANCEL", "MOVE HERE", "REFRESH");
    }

    fn draw_delete_prompt(
        frame: &mut Frame,
        item: &DirectoryItem,
        choice: DeleteChoice,
        battery_shown: bool,
    ) {
        header(
            frame,
            "DELETE?",
            if item.is_dir { "FOLDER" } else { "FILE" },
            battery_shown,
        );
        frame.render_widget(
            Paragraph::new(truncate(&item.name, 32))
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Yellow)),
            Rect::new(0, 2, 32, 1),
        );
        let warning = if item.is_dir {
            "All contents will be deleted"
        } else {
            "This cannot be undone"
        };
        frame.render_widget(
            Paragraph::new(warning)
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Red)),
            Rect::new(0, 4, 32, 1),
        );
        draw_menu_item(frame, 6, "Cancel", matches!(choice, DeleteChoice::Cancel));
        draw_menu_item(frame, 8, "Delete", matches!(choice, DeleteChoice::Delete));
        footer(frame, "CANCEL", "SELECT", "SELECT");
    }

    fn draw_editor(frame: &mut Frame, editor: &Editor, show_status: bool, battery_shown: bool) {
        let dirty = if editor.buffer.is_dirty() { "*" } else { "" };
        header(
            frame,
            // Truncate the name alone so the dirty marker survives long names.
            &format!("{}{dirty}", truncate(&editor.name, 16 - dirty.len())),
            if editor.keyboard_visible {
                "KEYS"
            } else {
                "MOVE"
            },
            battery_shown,
        );
        let visible_lines = if editor.keyboard_visible { 5 } else { 9 };
        let (cursor_line, cursor_col) = editor.buffer.display_position();
        for row in 0..visible_lines {
            let line_index = editor.scroll_line + row;
            if line_index >= editor.buffer.line_count() {
                break;
            }
            let mut text = editor
                .buffer
                .display_line(line_index, editor.horizontal, 32);
            if line_index == cursor_line
                && cursor_col >= editor.horizontal
                && cursor_col < editor.horizontal + 32
            {
                let pos = cursor_col - editor.horizontal;
                let mut chars: Vec<char> = text.chars().collect();
                while chars.len() <= pos {
                    chars.push(' ');
                }
                chars[pos] = '|';
                text = chars.into_iter().collect();
            }
            let style = if line_index == cursor_line {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new().fg(Color::White)
            };
            frame.render_widget(
                Paragraph::new(text).style(style),
                Rect::new(0, 1 + row as u16, 32, 1),
            );
        }
        if show_status {
            let status_y = if editor.keyboard_visible { 6 } else { 10 };
            let status = editor.status.clone().unwrap_or_else(|| {
                format!(
                    "Ln {} Col {}  {}/{}",
                    cursor_line + 1,
                    cursor_col + 1,
                    editor.buffer.bytes().len(),
                    MAX_DOCUMENT_BYTES
                )
            });
            frame.render_widget(
                Paragraph::new(truncate(&status, 32)).style(Style::new().fg(Color::DarkGray)),
                Rect::new(0, status_y, 32, 1),
            );
        }
        if editor.keyboard_visible {
            draw_keyboard(frame, &editor.keyboard, 7);
        }
        footer(frame, "EXIT", "KEYBOARD", "SAVE");

        if editor.media != EditorMedia::Ready {
            let message = match editor.media {
                EditorMedia::Removed => "SD removed\nReinsert original to save",
                EditorMedia::Different => "Different SD card\nSave disabled; exit/discard",
                EditorMedia::Ready => "",
            };
            let area = Rect::new(2, 4, 28, 3);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(message).alignment(Alignment::Center).style(
                    Style::new()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                area,
            );
        }
    }

    fn draw_keyboard(frame: &mut Frame, keyboard: &Keyboard, y: u16) {
        for row in 0..4 {
            let mut spans = Vec::new();
            for column in 0..keyboard.row_len(row) {
                let label = keyboard.label(row, column);
                let width = if row < 3 { 3 } else { 5 };
                let mut cell = String::new();
                let _ = write!(cell, "{:^width$}", label, width = width);
                let style = if keyboard.row == row && keyboard.column == column {
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::White).bg(Color::DarkGray)
                };
                spans.push(Span::styled(cell, style));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
                Rect::new(0, y + row as u16, 32, 1),
            );
        }
    }

    fn draw_exit_prompt(frame: &mut Frame, choice: ExitChoice, status: Option<&str>) {
        let area = Rect::new(1, 3, 30, 5);
        // Styling alone only recolors cells; the editor's glyphs underneath
        // (text, keyboard, media banner) would still show through the dialog.
        frame.render_widget(Clear, area);
        frame.render_widget(UiBlock::new().style(Style::new().bg(Color::Blue)), area);
        frame.render_widget(
            Paragraph::new("Exit editor?")
                .alignment(Alignment::Center)
                .style(
                    Style::new()
                        .fg(Color::White)
                        .bg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
            Rect::new(1, 3, 30, 1),
        );
        let options = [
            (ExitChoice::Save, " SAVE "),
            (ExitChoice::Discard, " DISCARD "),
            (ExitChoice::Cancel, " CANCEL "),
        ];
        let mut spans = Vec::new();
        for (option, label) in options {
            let selected = mem::discriminant(&option) == mem::discriminant(&choice);
            spans.push(Span::styled(
                label,
                if selected {
                    Style::new().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::new().fg(Color::White).bg(Color::DarkGray)
                },
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            Rect::new(1, 5, 30, 1),
        );
        // The dialog covers the editor's own status row, so a save failure
        // must be repeated inside the dialog to be seen at all.
        if let Some(status) = status {
            frame.render_widget(
                Paragraph::new(truncate(status, 30))
                    .alignment(Alignment::Center)
                    .style(Style::new().fg(Color::Yellow).bg(Color::Blue)),
                Rect::new(1, 6, 30, 1),
            );
        }
        frame.render_widget(
            Paragraph::new("Joystick + CLICK")
                .alignment(Alignment::Center)
                .style(Style::new().bg(Color::Blue)),
            Rect::new(1, 7, 30, 1),
        );
    }

    /// Row 0 layout: title in columns 0-15, mode hint right-aligned after it.
    /// While a battery chassis is connected its indicator owns columns 24-31
    /// and the hint stops at column 23; otherwise the hint extends to the
    /// top-right corner.
    fn header(frame: &mut Frame, left: &str, right: &str, battery_shown: bool) {
        frame.render_widget(
            Paragraph::new(left).style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Rect::new(0, 0, 16, 1),
        );
        let hint_width = if battery_shown { 8 } else { 16 };
        frame.render_widget(
            Paragraph::new(right)
                .alignment(Alignment::Right)
                .style(Style::new().fg(Color::DarkGray)),
            Rect::new(16, 0, hint_width, 1),
        );
    }

    fn footer(frame: &mut Frame, left: &str, middle: &str, right: &str) {
        let style = Style::new().fg(Color::Black).bg(Color::DarkGray);
        frame.render_widget(
            Paragraph::new(left)
                .alignment(Alignment::Center)
                .style(style),
            Rect::new(0, 11, 10, 1),
        );
        frame.render_widget(
            Paragraph::new(middle)
                .alignment(Alignment::Center)
                .style(style),
            Rect::new(10, 11, 11, 1),
        );
        frame.render_widget(
            Paragraph::new(right)
                .alignment(Alignment::Center)
                .style(style),
            Rect::new(21, 11, 11, 1),
        );
    }

    fn truncate(text: &str, width: usize) -> String {
        let mut result: String = text.chars().take(width).collect();
        if text.chars().count() > width && width > 0 {
            result.pop();
            result.push('~');
        }
        result
    }

    fn compact_size(bytes: u64) -> String {
        if bytes < 1_000 {
            format!("{bytes}B")
        } else if bytes < 1_000_000 {
            format!("{}K", bytes / 1_000)
        } else if bytes < 1_000_000_000 {
            format!("{}M", bytes / 1_000_000)
        } else {
            // One decimal keeps small gigabyte figures honest ("1.9G", not
            // "1G"); free space is the only value that reaches this range.
            let tenths = bytes / 100_000_000;
            format!("{}.{}G", tenths / 10, tenths % 10)
        }
    }
}
