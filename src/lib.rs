#![no_std]

extern crate alloc;

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;

use embedded_io::{ErrorType, Read, Seek, SeekFrom, Write};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024;
pub const MAX_FILE_STEM_CHARS: usize = 48;
pub const MAX_ENTRY_NAME_UTF16_UNITS: usize = 255;
pub const EXPLORER_PAGE_ROWS: usize = 8;

// ---------------------------------------------------------------------------
// Input

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    TopLeft,
    TopMiddle,
    TopRight,
    Up,
    Left,
    Click,
    Right,
    Down,
}

impl Button {
    pub const ALL: [Button; 8] = [
        Button::TopLeft,
        Button::TopMiddle,
        Button::TopRight,
        Button::Up,
        Button::Left,
        Button::Click,
        Button::Right,
        Button::Down,
    ];

    fn bit(self) -> u8 {
        1 << self as u8
    }

    fn repeats(self) -> bool {
        matches!(
            self,
            Button::Up | Button::Left | Button::Right | Button::Down
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawButtons(u8);

impl RawButtons {
    pub fn with(mut self, button: Button, pressed: bool) -> Self {
        if pressed {
            self.0 |= button.bit();
        } else {
            self.0 &= !button.bit();
        }
        self
    }

    pub fn pressed(self, button: Button) -> bool {
        self.0 & button.bit() != 0
    }
}

/// Debounces buttons and adds joystick key-repeat. Times are RTC ticks at 1024 Hz.
pub struct InputEngine {
    observed: RawButtons,
    stable: RawButtons,
    changed_at: [u32; 8],
    pressed_at: [u32; 8],
    next_repeat: [u32; 8],
}

impl InputEngine {
    pub const DEBOUNCE_TICKS: u32 = 21;
    pub const FIRST_REPEAT_TICKS: u32 = 410;
    pub const REPEAT_TICKS: u32 = 102;

    pub const fn new() -> Self {
        Self {
            observed: RawButtons(0),
            stable: RawButtons(0),
            changed_at: [0; 8],
            pressed_at: [0; 8],
            next_repeat: [0; 8],
        }
    }

    /// Returns at most one event per poll, in physical-control priority order.
    pub fn update(&mut self, raw: RawButtons, now: u32) -> Option<Button> {
        for button in Button::ALL {
            let i = button as usize;
            if raw.pressed(button) != self.observed.pressed(button) {
                self.observed = self.observed.with(button, raw.pressed(button));
                self.changed_at[i] = now;
            }
            if self.observed.pressed(button) != self.stable.pressed(button)
                && now.wrapping_sub(self.changed_at[i]) >= Self::DEBOUNCE_TICKS
            {
                let down = self.observed.pressed(button);
                self.stable = self.stable.with(button, down);
                if down {
                    self.pressed_at[i] = now;
                    self.next_repeat[i] = now.wrapping_add(Self::FIRST_REPEAT_TICKS);
                    return Some(button);
                }
            }
        }

        for button in Button::ALL {
            let i = button as usize;
            if button.repeats()
                && self.stable.pressed(button)
                && now.wrapping_sub(self.next_repeat[i]) < u32::MAX / 2
            {
                self.next_repeat[i] = now.wrapping_add(Self::REPEAT_TICKS);
                return Some(button);
            }
        }
        None
    }

    pub fn is_pressed(&self, button: Button) -> bool {
        self.stable.pressed(button)
    }

    pub fn held_ticks(&self, button: Button, now: u32) -> u32 {
        if self.is_pressed(button) {
            now.wrapping_sub(self.pressed_at[button as usize])
        } else {
            0
        }
    }
}

impl Default for InputEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Editor and on-screen keyboard

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewlineStyle {
    Lf,
    CrLf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    InvalidUtf8,
    TooLarge,
}

pub struct TextBuffer {
    bytes: Vec<u8>,
    cursor: usize,
    dirty: bool,
    newline: NewlineStyle,
}

impl TextBuffer {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, EditError> {
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(EditError::TooLarge);
        }
        core::str::from_utf8(&bytes).map_err(|_| EditError::InvalidUtf8)?;
        let newline = if bytes.windows(2).any(|pair| pair == b"\r\n") {
            NewlineStyle::CrLf
        } else {
            NewlineStyle::Lf
        };
        Ok(Self {
            bytes,
            cursor: 0,
            dirty: false,
            newline,
        })
    }

    pub fn empty() -> Self {
        Self::from_bytes(Vec::new()).expect("empty UTF-8 buffer")
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn set_cursor_end(&mut self) {
        self.cursor = self.bytes.len();
    }

    pub fn insert_char(&mut self, ch: char) -> Result<(), EditError> {
        let mut encoded = [0; 4];
        let encoded = ch.encode_utf8(&mut encoded).as_bytes();
        if self.bytes.len() + encoded.len() > MAX_DOCUMENT_BYTES {
            return Err(EditError::TooLarge);
        }
        self.bytes
            .splice(self.cursor..self.cursor, encoded.iter().copied());
        self.cursor += encoded.len();
        self.dirty = true;
        Ok(())
    }

    pub fn insert_newline(&mut self) -> Result<(), EditError> {
        let newline: &[u8] = match self.newline {
            NewlineStyle::Lf => b"\n",
            NewlineStyle::CrLf => b"\r\n",
        };
        if self.bytes.len() + newline.len() > MAX_DOCUMENT_BYTES {
            return Err(EditError::TooLarge);
        }
        self.bytes
            .splice(self.cursor..self.cursor, newline.iter().copied());
        self.cursor += newline.len();
        self.dirty = true;
        Ok(())
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.previous_boundary(self.cursor);
        self.bytes.drain(start..self.cursor);
        self.cursor = start;
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.bytes.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.bytes.drain(self.cursor..end);
        self.dirty = true;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.previous_boundary(self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn move_up(&mut self) {
        self.move_vertical(false);
    }

    pub fn move_down(&mut self) {
        self.move_vertical(true);
    }

    pub fn line_col(&self) -> (usize, usize) {
        let start = self.line_start(self.cursor);
        let line = self.bytes[..start].iter().filter(|&&b| b == b'\n').count();
        let col = core::str::from_utf8(&self.bytes[start..self.cursor])
            .unwrap_or("")
            .chars()
            .filter(|&ch| ch != '\r')
            .count();
        (line, col)
    }

    /// Cursor position in display cells, expanding tabs to four-column stops.
    pub fn display_position(&self) -> (usize, usize) {
        let (line, _) = self.line_col();
        let start = self.line_start(self.cursor);
        let text = core::str::from_utf8(&self.bytes[start..self.cursor]).unwrap_or("");
        let mut column = 0;
        for ch in text.chars() {
            match ch {
                '\t' => column += 4 - (column % 4),
                '\r' => {}
                _ => column += 1,
            }
        }
        (line, column)
    }

    pub fn line_count(&self) -> usize {
        1 + self.bytes.iter().filter(|&&b| b == b'\n').count()
    }

    pub fn display_line(&self, index: usize, horizontal: usize, width: usize) -> String {
        let (start, end) = self.line_range(index);
        let text = core::str::from_utf8(&self.bytes[start..end]).unwrap_or("");
        let mut expanded = String::new();
        let mut column = 0;
        for ch in text.chars() {
            if ch == '\t' {
                let spaces = 4 - (column % 4);
                for _ in 0..spaces {
                    expanded.push(' ');
                }
                column += spaces;
            } else if ch == '\r' {
                continue;
            } else {
                expanded.push(if ch.is_ascii() && !ch.is_control() {
                    ch
                } else {
                    '?'
                });
                column += 1;
            }
        }
        expanded.chars().skip(horizontal).take(width).collect()
    }

    fn previous_boundary(&self, at: usize) -> usize {
        if at == 0 {
            return 0;
        }
        if at >= 2 && &self.bytes[at - 2..at] == b"\r\n" {
            return at - 2;
        }
        let mut pos = at - 1;
        while pos > 0 && !core::str::from_utf8(&self.bytes[pos..at]).is_ok() {
            pos -= 1;
        }
        pos
    }

    fn next_boundary(&self, at: usize) -> usize {
        if at >= self.bytes.len() {
            return self.bytes.len();
        }
        if self.bytes[at..].starts_with(b"\r\n") {
            return at + 2;
        }
        let text = core::str::from_utf8(&self.bytes[at..]).unwrap_or("");
        at + text.chars().next().map(char::len_utf8).unwrap_or(0)
    }

    fn line_start(&self, at: usize) -> usize {
        self.bytes[..at]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        let end = self.bytes[at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.bytes.len(), |i| at + i);
        if end > at && self.bytes[end - 1] == b'\r' {
            end - 1
        } else {
            end
        }
    }

    fn line_range(&self, index: usize) -> (usize, usize) {
        let mut start = 0;
        for _ in 0..index {
            let Some(offset) = self.bytes[start..].iter().position(|&b| b == b'\n') else {
                return (self.bytes.len(), self.bytes.len());
            };
            start += offset + 1;
        }
        (start, self.line_end(start))
    }

    fn move_vertical(&mut self, down: bool) {
        let (line, column) = self.line_col();
        let target_line = if down {
            (line + 1).min(self.line_count() - 1)
        } else {
            line.saturating_sub(1)
        };
        if target_line == line {
            return;
        }
        let (start, end) = self.line_range(target_line);
        let text = core::str::from_utf8(&self.bytes[start..end]).unwrap_or("");
        self.cursor = text
            .char_indices()
            .nth(column)
            .map_or(end, |(offset, _)| start + offset);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardPage {
    Lower,
    Upper,
    Symbols,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Character(char),
    Case,
    Page,
    Space,
    Backspace,
    Delete,
    Enter,
}

pub struct Keyboard {
    pub page: KeyboardPage,
    pub row: usize,
    pub column: usize,
}

impl Keyboard {
    pub const fn new() -> Self {
        Self {
            page: KeyboardPage::Lower,
            row: 0,
            column: 0,
        }
    }

    pub fn selected(&self) -> Key {
        self.key_at(self.row, self.column)
    }

    pub fn row_len(&self, row: usize) -> usize {
        if row < 3 {
            10
        } else {
            6
        }
    }

    pub fn key_at(&self, row: usize, column: usize) -> Key {
        if row == 3 {
            return [
                Key::Case,
                Key::Page,
                Key::Space,
                Key::Backspace,
                Key::Delete,
                Key::Enter,
            ][column.min(5)];
        }
        let rows = match self.page {
            KeyboardPage::Lower => ["qwertyuiop", "asdfghjkl;", "zxcvbnm,./"],
            KeyboardPage::Upper => ["QWERTYUIOP", "ASDFGHJKL:", "ZXCVBNM?!\""],
            KeyboardPage::Symbols => ["1234567890", "-_=+[]{}()", "@#$%^&*'`~"],
        };
        Key::Character(rows[row.min(2)].chars().nth(column.min(9)).unwrap())
    }

    pub fn label(&self, row: usize, column: usize) -> String {
        match self.key_at(row, column) {
            Key::Character(ch) => ch.to_string(),
            Key::Case => "CASE".into(),
            Key::Page => "PAGE".into(),
            Key::Space => "SP".into(),
            Key::Backspace => "BS".into(),
            Key::Delete => "DEL".into(),
            Key::Enter => "ENT".into(),
        }
    }

    pub fn move_left(&mut self) {
        let len = self.row_len(self.row);
        self.column = (self.column + len - 1) % len;
    }

    pub fn move_right(&mut self) {
        self.column = (self.column + 1) % self.row_len(self.row);
    }

    pub fn move_up(&mut self) {
        self.row = (self.row + 3) % 4;
        self.column = self.column.min(self.row_len(self.row) - 1);
    }

    pub fn move_down(&mut self) {
        self.row = (self.row + 1) % 4;
        self.column = self.column.min(self.row_len(self.row) - 1);
    }

    pub fn activate_meta(&mut self, key: Key) {
        match key {
            Key::Case => {
                self.page = match self.page {
                    KeyboardPage::Lower => KeyboardPage::Upper,
                    KeyboardPage::Upper => KeyboardPage::Lower,
                    KeyboardPage::Symbols => KeyboardPage::Lower,
                }
            }
            Key::Page => {
                self.page = match self.page {
                    KeyboardPage::Lower => KeyboardPage::Upper,
                    KeyboardPage::Upper => KeyboardPage::Symbols,
                    KeyboardPage::Symbols => KeyboardPage::Lower,
                }
            }
            _ => {}
        }
    }
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

pub fn validate_file_stem(stem: &str) -> Result<(), &'static str> {
    if stem.is_empty() {
        return Err("Enter a file name");
    }
    if stem.chars().count() > MAX_FILE_STEM_CHARS {
        return Err("Name is longer than 48 characters");
    }
    validate_entry_name(stem)
}

pub fn validate_entry_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Enter a name");
    }
    if name.encode_utf16().count() > MAX_ENTRY_NAME_UTF16_UNITS {
        return Err("Name exceeds the FAT limit");
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err("Name cannot end with a space or dot");
    }
    if name.chars().any(|ch| {
        ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err("Name contains a FAT-reserved character");
    }
    Ok(())
}

pub fn is_txt_file(name: &str) -> bool {
    name.get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".txt"))
}

pub fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        alloc::format!("/{name}")
    } else {
        alloc::format!("{parent}/{name}")
    }
}

pub fn parent_path(path: &str) -> String {
    if path == "/" {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => path[..index].into(),
    }
}

// ---------------------------------------------------------------------------
// Raw SD byte stream and media probing

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SdStreamError {
    DeviceRead(String),
    DeviceWrite(String),
    DeviceSize(String),
    OutOfBounds,
}

impl fmt::Display for SdStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceRead(detail) => write!(f, "SD card read error: {detail}"),
            Self::DeviceWrite(detail) => write!(f, "SD card write error: {detail}"),
            Self::DeviceSize(detail) => write!(f, "SD card size error: {detail}"),
            Self::OutOfBounds => f.write_str("SD access outside media bounds"),
        }
    }
}

impl core::error::Error for SdStreamError {}

impl embedded_io::Error for SdStreamError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            Self::DeviceRead(_) | Self::DeviceWrite(_) | Self::DeviceSize(_) => {
                embedded_io::ErrorKind::Other
            }
            Self::OutOfBounds => embedded_io::ErrorKind::InvalidInput,
        }
    }
}

pub struct SdStream<D: BlockDevice> {
    device: D,
    position: u64,
    length: u64,
}

impl<D: BlockDevice> SdStream<D> {
    pub fn new(device: D) -> Result<Self, SdStreamError> {
        let BlockCount(blocks) = device
            .num_blocks()
            .map_err(|error| SdStreamError::DeviceSize(alloc::format!("{error:?}")))?;
        Ok(Self {
            device,
            position: 0,
            length: u64::from(blocks) * 512,
        })
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn into_inner(self) -> D {
        self.device
    }
}

impl<D: BlockDevice> ErrorType for SdStream<D> {
    type Error = SdStreamError;
}

impl<D: BlockDevice> Read for SdStream<D> {
    fn read(&mut self, output: &mut [u8]) -> Result<usize, Self::Error> {
        let available = self.length.saturating_sub(self.position);
        let wanted = output
            .len()
            .min(usize::try_from(available).unwrap_or(usize::MAX));
        let mut done = 0;
        while done < wanted {
            let sector_index = (self.position / 512) as u32;
            let sector_offset = (self.position % 512) as usize;
            let count = (wanted - done).min(512 - sector_offset);
            let mut block = Block::new();
            let mut failure = None;
            for _ in 0..3 {
                match self
                    .device
                    .read(core::slice::from_mut(&mut block), BlockIdx(sector_index))
                {
                    Ok(()) => {
                        failure = None;
                        break;
                    }
                    Err(error) => failure = Some(alloc::format!("{error:?}")),
                }
            }
            if let Some(detail) = failure {
                return Err(SdStreamError::DeviceRead(detail));
            }
            output[done..done + count]
                .copy_from_slice(&block.contents[sector_offset..sector_offset + count]);
            self.position += count as u64;
            done += count;
        }
        Ok(done)
    }
}

impl<D: BlockDevice> Write for SdStream<D> {
    fn write(&mut self, input: &[u8]) -> Result<usize, Self::Error> {
        if input.is_empty() {
            return Ok(0);
        }
        if self.position >= self.length {
            return Err(SdStreamError::OutOfBounds);
        }
        let wanted = input
            .len()
            .min(usize::try_from(self.length - self.position).unwrap_or(usize::MAX));
        let mut done = 0;
        while done < wanted {
            let sector_index = (self.position / 512) as u32;
            let sector_offset = (self.position % 512) as usize;
            let count = (wanted - done).min(512 - sector_offset);
            let mut block = Block::new();
            if sector_offset != 0 || count != 512 {
                let mut failure = None;
                for _ in 0..3 {
                    match self
                        .device
                        .read(core::slice::from_mut(&mut block), BlockIdx(sector_index))
                    {
                        Ok(()) => {
                            failure = None;
                            break;
                        }
                        Err(error) => failure = Some(alloc::format!("{error:?}")),
                    }
                }
                if let Some(detail) = failure {
                    return Err(SdStreamError::DeviceRead(detail));
                }
            }
            block.contents[sector_offset..sector_offset + count]
                .copy_from_slice(&input[done..done + count]);
            let mut failure = None;
            for _ in 0..3 {
                match self
                    .device
                    .write(core::slice::from_ref(&block), BlockIdx(sector_index))
                {
                    Ok(()) => {
                        failure = None;
                        break;
                    }
                    Err(error) => failure = Some(alloc::format!("{error:?}")),
                }
            }
            if let Some(detail) = failure {
                return Err(SdStreamError::DeviceWrite(detail));
            }
            self.position += count as u64;
            done += count;
        }
        Ok(done)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<D: BlockDevice> Seek for SdStream<D> {
    fn seek(&mut self, from: SeekFrom) -> Result<u64, Self::Error> {
        let next = match from {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        if next < 0 || next > i128::from(self.length) {
            return Err(SdStreamError::OutOfBounds);
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaLayout {
    pub start_lba: u32,
    pub sector_count: u32,
    pub volume_serial: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProbeError<E> {
    Io(E),
    Unsupported,
    Invalid,
}

pub fn probe_fat<S: Read + Seek>(
    storage: &mut S,
    media_sectors: u32,
) -> Result<MediaLayout, ProbeError<S::Error>> {
    let mut sector = [0u8; 512];
    if !read_sector(storage, 0, &mut sector).map_err(ProbeError::Io)? {
        return Err(ProbeError::Invalid);
    }
    if let Some((serial, total)) = parse_fat_boot_sector(&sector) {
        if total > media_sectors {
            return Err(ProbeError::Invalid);
        }
        return Ok(MediaLayout {
            start_lba: 0,
            sector_count: total,
            volume_serial: serial,
        });
    }
    if sector[510..512] != [0x55, 0xaa] {
        return Err(ProbeError::Invalid);
    }
    let mut saw_unsupported = false;
    for index in 0..4 {
        let base = 446 + index * 16;
        let kind = sector[base + 4];
        let start = le_u32(&sector[base + 8..base + 12]);
        let count = le_u32(&sector[base + 12..base + 16]);
        if matches!(kind, 0x07 | 0xee | 0x05 | 0x0f) {
            saw_unsupported = true;
            continue;
        }
        if !matches!(kind, 0x04 | 0x06 | 0x0e | 0x0b | 0x0c) || count == 0 {
            continue;
        }
        if start
            .checked_add(count)
            .is_none_or(|end| end > media_sectors)
        {
            return Err(ProbeError::Invalid);
        }
        if !read_sector(storage, start, &mut sector).map_err(ProbeError::Io)? {
            return Err(ProbeError::Invalid);
        }
        if let Some((serial, total)) = parse_fat_boot_sector(&sector) {
            if total > count {
                return Err(ProbeError::Invalid);
            }
            return Ok(MediaLayout {
                start_lba: start,
                sector_count: total,
                volume_serial: serial,
            });
        }
        return Err(ProbeError::Invalid);
    }
    if saw_unsupported {
        Err(ProbeError::Unsupported)
    } else {
        Err(ProbeError::Invalid)
    }
}

fn parse_fat_boot_sector(sector: &[u8; 512]) -> Option<(u32, u32)> {
    if &sector[3..11] == b"EXFAT   " || sector[510..512] != [0x55, 0xaa] {
        return None;
    }
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    let sectors_per_cluster = sector[13];
    let reserved = u16::from_le_bytes([sector[14], sector[15]]) as u32;
    let fats = sector[16] as u32;
    let root_entries = u16::from_le_bytes([sector[17], sector[18]]) as u32;
    let total16 = u16::from_le_bytes([sector[19], sector[20]]) as u32;
    let total32 = le_u32(&sector[32..36]);
    let fat16 = u16::from_le_bytes([sector[22], sector[23]]) as u32;
    let fat32 = le_u32(&sector[36..40]);
    let total = if total16 != 0 { total16 } else { total32 };
    let fat_size = if fat16 != 0 { fat16 } else { fat32 };
    if bytes_per_sector != 512
        || sectors_per_cluster == 0
        || !sectors_per_cluster.is_power_of_two()
        || reserved == 0
        || fats == 0
        || total == 0
        || fat_size == 0
    {
        return None;
    }
    let root_sectors = (root_entries * 32).div_ceil(512);
    let overhead = reserved
        .checked_add(fats.checked_mul(fat_size)?)?
        .checked_add(root_sectors)?;
    let clusters = total.checked_sub(overhead)? / sectors_per_cluster as u32;
    if clusters < 4_085 {
        return None; // FAT12
    }
    if clusters < 65_525 {
        Some((le_u32(&sector[39..43]), total))
    } else if root_entries == 0 && fat16 == 0 && le_u32(&sector[44..48]) >= 2 {
        Some((le_u32(&sector[67..71]), total))
    } else {
        None
    }
}

fn read_sector<S: Read + Seek>(
    storage: &mut S,
    lba: u32,
    sector: &mut [u8; 512],
) -> Result<bool, S::Error> {
    storage.seek(SeekFrom::Start(u64::from(lba) * 512))?;
    let mut done = 0;
    while done < sector.len() {
        let count = storage.read(&mut sector[done..])?;
        if count == 0 {
            return Ok(false);
        }
        done += count;
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// FAT32 formatter

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatGeometry {
    pub partition_start: u32,
    pub partition_sectors: u32,
    pub sectors_per_cluster: u8,
    pub fat_sectors: u32,
    pub cluster_count: u32,
    pub volume_serial: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError<E> {
    Io(E),
    TooSmall,
    InvalidGeometry,
    VerifyFailed,
}

pub fn fat32_geometry(
    media_sectors: u32,
) -> Result<FormatGeometry, FormatError<core::convert::Infallible>> {
    const PARTITION_START: u32 = 2_048;
    if media_sectors <= PARTITION_START + 65_525 + 64 {
        return Err(FormatError::TooSmall);
    }
    let partition_sectors = media_sectors - PARTITION_START;
    let preferred: u32 = if partition_sectors <= 532_480 {
        1
    } else if partition_sectors <= 16_777_216 {
        8
    } else if partition_sectors <= 33_554_432 {
        16
    } else if partition_sectors <= 67_108_864 {
        32
    } else {
        64
    };
    let mut candidates = [64u8, 32, 16, 8, 4, 2, 1];
    candidates.sort_unstable_by_key(|spc| preferred.abs_diff(*spc as u32));
    for sectors_per_cluster in candidates {
        let spc = sectors_per_cluster as u32;
        let mut fat_sectors = 1u32;
        for _ in 0..32 {
            let overhead = 32u32.saturating_add(2u32.saturating_mul(fat_sectors));
            if overhead >= partition_sectors {
                break;
            }
            let clusters = (partition_sectors - overhead) / spc;
            let next = (u64::from(clusters + 2) * 4).div_ceil(512) as u32;
            if next == fat_sectors {
                if (65_525..=0x0fff_fff5).contains(&clusters) {
                    return Ok(FormatGeometry {
                        partition_start: PARTITION_START,
                        partition_sectors,
                        sectors_per_cluster,
                        fat_sectors,
                        cluster_count: clusters,
                        volume_serial: 0x5749_4f00 ^ media_sectors,
                    });
                }
                break;
            }
            fat_sectors = next;
        }
    }
    Err(FormatError::InvalidGeometry)
}

pub fn format_fat32<S: Read + Write + Seek>(
    storage: &mut S,
    media_sectors: u32,
) -> Result<FormatGeometry, FormatError<S::Error>> {
    let geometry = fat32_geometry(media_sectors).map_err(|error| match error {
        FormatError::TooSmall => FormatError::TooSmall,
        _ => FormatError::InvalidGeometry,
    })?;
    let mut sector = [0u8; 512];

    // MBR and its single LBA-addressed FAT32 partition.
    sector[446 + 1..446 + 4].copy_from_slice(&[0xfe, 0xff, 0xff]);
    sector[446 + 4] = 0x0c;
    sector[446 + 5..446 + 8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    put_u32(&mut sector[446 + 8..446 + 12], geometry.partition_start);
    put_u32(&mut sector[446 + 12..446 + 16], geometry.partition_sectors);
    sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    write_sector(storage, 0, &sector).map_err(FormatError::Io)?;

    let boot = make_boot_sector(&geometry);
    let info = make_fsinfo(&geometry);
    let zero = [0u8; 512];
    for offset in 0..32 {
        write_sector(storage, geometry.partition_start + offset, &zero).map_err(FormatError::Io)?;
    }
    write_sector(storage, geometry.partition_start, &boot).map_err(FormatError::Io)?;
    write_sector(storage, geometry.partition_start + 1, &info).map_err(FormatError::Io)?;
    write_sector(storage, geometry.partition_start + 6, &boot).map_err(FormatError::Io)?;
    write_sector(storage, geometry.partition_start + 7, &info).map_err(FormatError::Io)?;

    let first_fat = geometry.partition_start + 32;
    for copy in 0..2 {
        let base = first_fat + copy * geometry.fat_sectors;
        for offset in 0..geometry.fat_sectors {
            write_sector(storage, base + offset, &zero).map_err(FormatError::Io)?;
        }
        let mut first = [0u8; 512];
        put_u32(&mut first[0..4], 0x0fff_fff8);
        put_u32(&mut first[4..8], 0xffff_ffff);
        put_u32(&mut first[8..12], 0x0fff_ffff);
        write_sector(storage, base, &first).map_err(FormatError::Io)?;
    }

    let root = first_fat + 2 * geometry.fat_sectors;
    for offset in 0..u32::from(geometry.sectors_per_cluster) {
        write_sector(storage, root + offset, &zero).map_err(FormatError::Io)?;
    }
    storage.flush().map_err(FormatError::Io)?;

    let mut verify = [0u8; 512];
    if !read_sector(storage, 0, &mut verify).map_err(FormatError::Io)? {
        return Err(FormatError::VerifyFailed);
    }
    if verify[510..512] != [0x55, 0xaa] || le_u32(&verify[454..458]) != geometry.partition_start {
        return Err(FormatError::VerifyFailed);
    }
    if !read_sector(storage, geometry.partition_start, &mut verify).map_err(FormatError::Io)? {
        return Err(FormatError::VerifyFailed);
    }
    if verify[510..512] != [0x55, 0xaa] || &verify[82..90] != b"FAT32   " {
        return Err(FormatError::VerifyFailed);
    }
    if !read_sector(storage, geometry.partition_start + 1, &mut verify).map_err(FormatError::Io)? {
        return Err(FormatError::VerifyFailed);
    }
    if le_u32(&verify[0..4]) != 0x4161_5252 || le_u32(&verify[484..488]) != 0x6141_7272 {
        return Err(FormatError::VerifyFailed);
    }
    Ok(geometry)
}

fn make_boot_sector(g: &FormatGeometry) -> [u8; 512] {
    let mut b = [0u8; 512];
    b[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"WIOEDIT ");
    put_u16(&mut b[11..13], 512);
    b[13] = g.sectors_per_cluster;
    put_u16(&mut b[14..16], 32);
    b[16] = 2;
    b[21] = 0xf8;
    put_u32(&mut b[28..32], g.partition_start);
    put_u16(&mut b[24..26], 63);
    put_u16(&mut b[26..28], 255);
    put_u32(&mut b[32..36], g.partition_sectors);
    put_u32(&mut b[36..40], g.fat_sectors);
    put_u32(&mut b[44..48], 2);
    put_u16(&mut b[48..50], 1);
    put_u16(&mut b[50..52], 6);
    b[64] = 0x80;
    b[66] = 0x29;
    put_u32(&mut b[67..71], g.volume_serial);
    b[71..82].copy_from_slice(b"WIO-TERM   ");
    b[82..90].copy_from_slice(b"FAT32   ");
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    b
}

fn make_fsinfo(g: &FormatGeometry) -> [u8; 512] {
    let mut b = [0u8; 512];
    put_u32(&mut b[0..4], 0x4161_5252);
    put_u32(&mut b[484..488], 0x6141_7272);
    put_u32(&mut b[488..492], g.cluster_count.saturating_sub(1));
    put_u32(&mut b[492..496], 3);
    put_u32(&mut b[508..512], 0xaa55_0000);
    b
}

fn write_sector<S: Write + Seek>(
    storage: &mut S,
    lba: u32,
    sector: &[u8; 512],
) -> Result<(), S::Error> {
    storage.seek(SeekFrom::Start(u64::from(lba) * 512))?;
    storage.write_all(sector)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte field"))
}

fn put_u16(bytes: &mut [u8], value: u16) {
    bytes.copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], value: u32) {
    bytes.copy_from_slice(&value.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Filesystem helpers

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub hidden: bool,
    pub system: bool,
}

pub fn read_directory_page<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<DirectoryItem>, usize), unifat::FsError<S::Error>> {
    let mut page = Vec::with_capacity(limit);
    let mut total = 0;
    for result in volume.read_dir(path)? {
        let entry = result?;
        if total >= offset && page.len() < limit {
            let meta = entry.metadata();
            page.push(DirectoryItem {
                name: entry.file_name().into(),
                path: entry.path().as_str().replace('\\', "/"),
                is_dir: entry.is_dir(),
                size: meta.len(),
                hidden: meta.is_hidden(),
                system: meta.is_system(),
            });
        }
        total += 1;
    }
    Ok((page, total))
}

pub fn read_directory_folders_page<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    offset: usize,
    limit: usize,
    excluded_path: Option<&str>,
) -> Result<(Vec<DirectoryItem>, usize), unifat::FsError<S::Error>> {
    let mut page = Vec::with_capacity(limit);
    let mut total = 0;
    for result in volume.read_dir(path)? {
        let entry = result?;
        let entry_path = entry.path().as_str().replace('\\', "/");
        if !entry.is_dir()
            || excluded_path.is_some_and(|excluded| entry_path.eq_ignore_ascii_case(excluded))
        {
            continue;
        }
        if total >= offset && page.len() < limit {
            let meta = entry.metadata();
            page.push(DirectoryItem {
                name: entry.file_name().into(),
                path: entry_path,
                is_dir: true,
                size: meta.len(),
                hidden: meta.is_hidden(),
                system: meta.is_system(),
            });
        }
        total += 1;
    }
    Ok((page, total))
}

#[derive(Debug)]
pub enum LoadError<E: embedded_io::Error> {
    Filesystem(unifat::FsError<E>),
    TooLarge,
    InvalidUtf8,
}

pub fn load_text<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
) -> Result<TextBuffer, LoadError<S::Error>> {
    let metadata = volume.metadata(path).map_err(LoadError::Filesystem)?;
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(LoadError::TooLarge);
    }
    let bytes = volume.read(path).map_err(LoadError::Filesystem)?;
    TextBuffer::from_bytes(bytes).map_err(|error| match error {
        EditError::TooLarge => LoadError::TooLarge,
        EditError::InvalidUtf8 => LoadError::InvalidUtf8,
    })
}

pub fn create_empty<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
) -> Result<(), unifat::FsError<S::Error>> {
    match volume.metadata(path) {
        Ok(_) => return Err(unifat::FsError::AlreadyExists),
        Err(unifat::FsError::NotFound) => {}
        Err(error) => return Err(error),
    }
    write_verified(volume, path, b"")
}

pub fn create_directory_verified<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
) -> Result<(), unifat::FsError<S::Error>> {
    match volume.metadata(path) {
        Ok(_) => return Err(unifat::FsError::AlreadyExists),
        Err(unifat::FsError::NotFound) => {}
        Err(error) => return Err(error),
    }
    volume.create_dir(path)?;
    match volume.metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(unifat::FsError::Corrupt(unifat::CorruptKind::Other)),
        Err(error) => Err(error),
    }
}

pub fn rename_verified<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    from: &str,
    to: &str,
) -> Result<(), unifat::FsError<S::Error>> {
    volume.metadata(from)?;
    volume.rename(from, to)?;
    let destination_present = exact_entry_exists(volume, to)?;
    let source_present = exact_entry_exists(volume, from)?;
    if destination_present && (from == to || !source_present) {
        Ok(())
    } else {
        Err(unifat::FsError::Corrupt(unifat::CorruptKind::Other))
    }
}

pub fn delete_verified<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    is_dir: bool,
) -> Result<(), unifat::FsError<S::Error>> {
    let metadata = volume.metadata(path)?;
    if metadata.is_dir() != is_dir {
        return Err(if is_dir {
            unifat::FsError::NotADirectory
        } else {
            unifat::FsError::IsADirectory
        });
    }
    if is_dir {
        volume.remove_dir_all(path)?;
    } else {
        volume.remove_file(path)?;
    }
    match volume.metadata(path) {
        Err(unifat::FsError::NotFound) => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(unifat::FsError::Corrupt(unifat::CorruptKind::Other)),
    }
}

fn exact_entry_exists<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
) -> Result<bool, unifat::FsError<S::Error>> {
    let parent = parent_path(path);
    let name = path.rsplit('/').next().unwrap_or("");
    for entry in volume.read_dir(&parent)? {
        if entry?.file_name() == name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Write a complete file without overriding storage or flush errors with a
/// follow-up read from the same mounted filesystem. Its metadata cache may
/// contain the intended state even when that state did not reach the card.
fn write_verified<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    contents: &[u8],
) -> Result<(), unifat::FsError<S::Error>> {
    volume.write(path, contents)
}

/// Replace a file through one filesystem write and verify every byte by
/// reading it back. This minimizes SD metadata writes while still refusing to
/// report success for a partial or corrupt write.
pub fn save_verified<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    contents: &[u8],
) -> Result<(), unifat::FsError<S::Error>> {
    write_verified(volume, path, contents)?;
    match volume.read(path) {
        Ok(actual) if actual == contents => Ok(()),
        Ok(_) => Err(unifat::FsError::Corrupt(unifat::CorruptKind::Other)),
        Err(error) => Err(error),
    }
}

/// Reduce the nested filesystem/partition/device error to a message that fits
/// the Wio Terminal's 32-column status line.
pub fn save_failure_reason(
    error: &unifat::FsError<unifat::PartitionError<SdStreamError>>,
) -> String {
    use unifat::{FsError, PartitionError};

    match error {
        FsError::Io(PartitionError::Io(SdStreamError::DeviceRead(detail))) => {
            compact_device_error(detail, "read")
        }
        FsError::Io(PartitionError::Io(SdStreamError::DeviceWrite(detail))) => {
            compact_device_error(detail, "write")
        }
        FsError::Io(PartitionError::Io(SdStreamError::DeviceSize(detail))) => {
            compact_device_error(detail, "size")
        }
        FsError::Io(PartitionError::Io(SdStreamError::OutOfBounds)) => "SD bounds error".into(),
        FsError::Io(PartitionError::OutOfBounds) => "partition bounds error".into(),
        FsError::NotFound => "file not found".into(),
        FsError::AlreadyExists => "file already exists".into(),
        FsError::NotADirectory => "parent is not a folder".into(),
        FsError::IsADirectory => "path is a folder".into(),
        FsError::DirectoryNotEmpty => "folder is not empty".into(),
        FsError::FileLocked => "file is locked".into(),
        FsError::StorageFull => "storage is full".into(),
        FsError::RootDirectoryFull => "directory is full".into(),
        FsError::InvalidInput => "invalid file path".into(),
        FsError::FileTooLarge => "file is too large".into(),
        FsError::ReadOnlyFile => "file is read-only".into(),
        FsError::PermissionDenied => "permission denied".into(),
        FsError::Corrupt(_) => "filesystem is corrupt".into(),
        FsError::Unsupported => "unsupported filesystem".into(),
        _ => "filesystem error".into(),
    }
}

fn compact_device_error(detail: &str, operation: &str) -> String {
    match detail {
        "WriteError" => "card rejected write".into(),
        "ReadError" => "card rejected read".into(),
        "Transport" | "GpioError" => "SD SPI transport error".into(),
        "TimeoutWaitNotBusy" => "SD write timed out".into(),
        "TimeoutReadBuffer" => "SD read timed out".into(),
        "CardNotFound" => "SD card not found".into(),
        "BadState" => "SD card bad state".into(),
        "CantEnableCRC" => "SD CRC unavailable".into(),
        "Cmd58Error" | "RegisterReadError" => "SD register error".into(),
        _ if detail.starts_with("CrcError") => "SD CRC mismatch".into(),
        _ if detail.starts_with("TimeoutCommand") || detail.starts_with("TimeoutACommand") => {
            "SD command timed out".into()
        }
        _ => alloc::format!("SD {operation}: {detail}"),
    }
}

pub fn save_transactional<S: Read + Write + Seek>(
    volume: &unifat::Volume<S>,
    path: &str,
    contents: &[u8],
) -> Result<(), unifat::FsError<S::Error>> {
    let directory = parent_path(path);
    let existed = match volume.metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Err(unifat::FsError::IsADirectory),
        Ok(metadata) if metadata.is_read_only() => {
            return Err(unifat::FsError::ReadOnlyFile);
        }
        Ok(_) => true,
        Err(unifat::FsError::NotFound) => false,
        Err(error) => return Err(error),
    };

    let mut suffix = 0u16;
    let (temporary, backup) = loop {
        let temporary = join_path(&directory, &alloc::format!("~WIO{suffix:04X}.TMP"));
        let backup = join_path(&directory, &alloc::format!("~WIO{suffix:04X}.BAK"));
        let temp_free = match volume.metadata(&temporary) {
            Err(unifat::FsError::NotFound) => true,
            Ok(_) => false,
            Err(error) => return Err(error),
        };
        let backup_free = match volume.metadata(&backup) {
            Err(unifat::FsError::NotFound) => true,
            Ok(_) => false,
            Err(error) => return Err(error),
        };
        if temp_free && backup_free {
            break (temporary, backup);
        }
        suffix = suffix.checked_add(1).ok_or(unifat::FsError::StorageFull)?;
    };

    if let Err(error) = save_verified(volume, &temporary, contents) {
        if !matches!(&error, unifat::FsError::Io(_)) {
            let _ = volume.remove_file(&temporary);
        }
        return Err(error);
    }
    if existed {
        if let Err(error) = volume.rename(path, &backup) {
            // Do not perform more mutations after an I/O error: the mounted
            // metadata cache no longer proves what reached the card.
            if !matches!(&error, unifat::FsError::Io(_)) {
                let _ = volume.remove_file(&temporary);
            }
            return Err(error);
        }
    }
    if let Err(error) = volume.rename(&temporary, path) {
        if !matches!(&error, unifat::FsError::Io(_)) {
            if existed {
                let _ = volume.rename(&backup, path);
            }
            let _ = volume.remove_file(&temporary);
        }
        return Err(error);
    }
    match volume.read(path) {
        Ok(actual) if actual == contents => {}
        Ok(_) => return Err(unifat::FsError::Corrupt(unifat::CorruptKind::Other)),
        Err(error) => return Err(error),
    }
    if existed {
        // The final name now contains verified data. A stale backup is much
        // less harmful than telling the user the save failed (or rolling back
        // good data) solely because cleanup had a transient card error.
        let _ = volume.remove_file(&backup);
    }
    Ok(())
}

/// A cloneable adapter useful when a board-specific controller must be shared
/// between the mounted filesystem and card-removal handling.
pub struct SharedBlockDevice<D>(pub Rc<RefCell<D>>);

impl<D> Clone for SharedBlockDevice<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<D: BlockDevice> BlockDevice for SharedBlockDevice<D> {
    type Error = D::Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        self.0.borrow().read(blocks, start)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        self.0.borrow().write(blocks, start)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.0.borrow().num_blocks()
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::cell::{Cell, RefCell};

    #[test]
    fn input_is_debounced_and_directions_repeat() {
        let mut input = InputEngine::new();
        let down = RawButtons::default().with(Button::Down, true);
        assert_eq!(input.update(down, 0), None);
        assert_eq!(input.update(down, 20), None);
        assert_eq!(input.update(down, 21), Some(Button::Down));
        assert_eq!(input.update(down, 430), None);
        assert_eq!(input.update(down, 431), Some(Button::Down));
    }

    #[test]
    fn editor_preserves_crlf_and_utf8_boundaries() {
        let mut editor = TextBuffer::from_bytes("aé\r\nb".as_bytes().to_vec()).unwrap();
        editor.move_right();
        editor.move_right();
        assert_eq!(editor.cursor(), 3);
        editor.insert_newline().unwrap();
        assert_eq!(editor.bytes(), "aé\r\n\r\nb".as_bytes());
        editor.backspace();
        assert_eq!(editor.bytes(), "aé\r\nb".as_bytes());
    }

    #[test]
    fn editor_moves_between_logical_lines() {
        let mut editor = TextBuffer::from_bytes(b"abc\nxy\n12345".to_vec()).unwrap();
        for _ in 0..3 {
            editor.move_right();
        }
        editor.move_down();
        assert_eq!(editor.line_col(), (1, 2));
        editor.move_down();
        assert_eq!(editor.line_col(), (2, 2));
        editor.move_up();
        assert_eq!(editor.line_col(), (1, 2));
    }

    #[test]
    fn editor_reports_tab_expanded_display_column() {
        let mut editor = TextBuffer::from_bytes(b"a\tb".to_vec()).unwrap();
        editor.move_right();
        editor.move_right();
        assert_eq!(editor.line_col(), (0, 2));
        assert_eq!(editor.display_position(), (0, 4));
        assert_eq!(editor.display_line(0, 0, 8), "a   b");
    }

    #[test]
    fn validates_names_and_extensions() {
        assert!(validate_file_stem("Trip notes (2)").is_ok());
        assert!(validate_file_stem("bad:name").is_err());
        assert!(validate_file_stem("trailing.").is_err());
        assert!(validate_entry_name("Projects").is_ok());
        assert!(validate_entry_name("").is_err());
        assert!(validate_entry_name("bad/name").is_err());
        assert!(validate_entry_name(&"a".repeat(MAX_ENTRY_NAME_UTF16_UNITS + 1)).is_err());
        assert!(is_txt_file("NOTES.TxT"));
        assert!(!is_txt_file("notes.md"));
    }

    #[test]
    fn computes_valid_fat32_geometry() {
        let geometry = fat32_geometry(131_072).unwrap();
        assert!(geometry.cluster_count >= 65_525);
        assert!(geometry.sectors_per_cluster.is_power_of_two());
        assert!(geometry.partition_start + geometry.partition_sectors <= 131_072);
    }

    struct MockBlocks {
        blocks: RefCell<Vec<Block>>,
    }

    impl MockBlocks {
        fn new(count: usize) -> Self {
            Self {
                blocks: RefCell::new(vec![Block::new(); count]),
            }
        }
    }

    impl BlockDevice for MockBlocks {
        type Error = ();

        fn read(&self, output: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
            let blocks = self.blocks.borrow();
            output.clone_from_slice(&blocks[start.0 as usize..start.0 as usize + output.len()]);
            Ok(())
        }

        fn write(&self, input: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
            let mut blocks = self.blocks.borrow_mut();
            blocks[start.0 as usize..start.0 as usize + input.len()].clone_from_slice(input);
            Ok(())
        }

        fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
            Ok(BlockCount(self.blocks.borrow().len() as u32))
        }
    }

    #[test]
    fn sd_stream_handles_partial_cross_sector_io() {
        let mut stream = SdStream::new(MockBlocks::new(3)).unwrap();
        stream.seek(SeekFrom::Start(510)).unwrap();
        stream.write_all(b"abcdef").unwrap();
        stream.seek(SeekFrom::Start(510)).unwrap();
        let mut result = [0; 6];
        assert_eq!(stream.read(&mut result).unwrap(), 6);
        assert_eq!(&result, b"abcdef");
        assert_eq!(
            stream.seek(SeekFrom::End(1)),
            Err(SdStreamError::OutOfBounds)
        );
    }

    #[test]
    fn editor_enforces_the_byte_limit() {
        let mut editor = TextBuffer::from_bytes(vec![b'a'; MAX_DOCUMENT_BYTES]).unwrap();
        assert_eq!(editor.insert_char('b'), Err(EditError::TooLarge));
        assert_eq!(
            TextBuffer::from_bytes(vec![b'a'; MAX_DOCUMENT_BYTES + 1]).err(),
            Some(EditError::TooLarge)
        );
    }

    struct RamDisk {
        bytes: Vec<u8>,
        position: u64,
        fail_writes: Rc<Cell<bool>>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct RamError;

    impl fmt::Display for RamError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("RAM disk error")
        }
    }
    impl core::error::Error for RamError {}
    impl embedded_io::Error for RamError {
        fn kind(&self) -> embedded_io::ErrorKind {
            embedded_io::ErrorKind::Other
        }
    }
    impl ErrorType for RamDisk {
        type Error = RamError;
    }
    impl Read for RamDisk {
        fn read(&mut self, out: &mut [u8]) -> Result<usize, RamError> {
            let start = self.position as usize;
            let count = out.len().min(self.bytes.len().saturating_sub(start));
            out[..count].copy_from_slice(&self.bytes[start..start + count]);
            self.position += count as u64;
            Ok(count)
        }
    }
    impl Write for RamDisk {
        fn write(&mut self, input: &[u8]) -> Result<usize, RamError> {
            if self.fail_writes.get() {
                return Err(RamError);
            }
            let start = self.position as usize;
            if start + input.len() > self.bytes.len() {
                return Err(RamError);
            }
            self.bytes[start..start + input.len()].copy_from_slice(input);
            self.position += input.len() as u64;
            Ok(input.len())
        }
        fn flush(&mut self) -> Result<(), RamError> {
            Ok(())
        }
    }
    impl Seek for RamDisk {
        fn seek(&mut self, from: SeekFrom) -> Result<u64, RamError> {
            let next = match from {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::Current(n) => self.position as i64 + n,
                SeekFrom::End(n) => self.bytes.len() as i64 + n,
            };
            if next < 0 || next as usize > self.bytes.len() {
                return Err(RamError);
            }
            self.position = next as u64;
            Ok(self.position)
        }
    }

    #[test]
    fn formatted_media_mounts_and_supports_long_names() {
        let sectors = 131_072u32;
        let mut disk = RamDisk {
            bytes: vec![0; sectors as usize * 512],
            position: 0,
            fail_writes: Rc::new(Cell::new(false)),
        };
        let fail_writes = disk.fail_writes.clone();
        let geometry = format_fat32(&mut disk, sectors).unwrap();
        disk.position = 0;
        let layout = probe_fat(&mut disk, sectors).unwrap();
        assert_eq!(layout.start_lba, geometry.partition_start);
        let partition = unifat::Partition::new(disk, layout.start_lba, layout.sector_count);
        let volume = unifat::Volume::mount_with(
            partition,
            unifat::FsOptions::new().with_auto_timestamps(false),
        )
        .unwrap();
        volume.write("/A long file name.txt", b"hello").unwrap();
        assert_eq!(volume.read("/A long file name.txt").unwrap(), b"hello");
        save_transactional(&volume, "/A long file name.txt", b"replacement").unwrap();
        assert_eq!(
            volume.read("/A long file name.txt").unwrap(),
            b"replacement"
        );
        save_verified(&volume, "/A long file name.txt", b"direct save").unwrap();
        assert_eq!(
            volume.read("/A long file name.txt").unwrap(),
            b"direct save"
        );
        create_empty(&volume, "/Created empty.txt").unwrap();
        save_verified(&volume, "/Created empty.txt", b"first contents").unwrap();
        assert_eq!(
            volume.read("/Created empty.txt").unwrap(),
            b"first contents"
        );
        assert!(matches!(
            create_empty(&volume, "/A long file name.txt"),
            Err(unifat::FsError::AlreadyExists)
        ));
        let names: Vec<String> = volume
            .read_dir("/")
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into())
            .collect();
        assert_eq!(
            names,
            vec![
                String::from("A long file name.txt"),
                String::from("Created empty.txt")
            ]
        );

        create_directory_verified(&volume, "/Projects").unwrap();
        create_directory_verified(&volume, "/Archive").unwrap();
        create_directory_verified(&volume, "/Projects/Nested").unwrap();
        create_empty(&volume, "/Projects/Nested/readme.txt").unwrap();
        assert!(matches!(
            create_directory_verified(&volume, "/Projects"),
            Err(unifat::FsError::AlreadyExists)
        ));

        let (folders, total) =
            read_directory_folders_page(&volume, "/", 0, 8, Some("/Archive")).unwrap();
        assert_eq!(total, 1);
        assert_eq!(folders[0].name, "Projects");

        rename_verified(&volume, "/Projects", "/PROJECTS").unwrap();
        rename_verified(&volume, "/Created empty.txt", "/PROJECTS/Created empty.txt").unwrap();
        assert_eq!(
            volume.read("/PROJECTS/Created empty.txt").unwrap(),
            b"first contents"
        );
        assert!(matches!(
            rename_verified(
                &volume,
                "/A long file name.txt",
                "/PROJECTS/Created empty.txt"
            ),
            Err(unifat::FsError::AlreadyExists)
        ));
        assert!(matches!(
            rename_verified(&volume, "/PROJECTS", "/PROJECTS/Nested/PROJECTS"),
            Err(unifat::FsError::InvalidInput)
        ));
        rename_verified(&volume, "/PROJECTS/Nested", "/Archive/Nested").unwrap();
        assert_eq!(volume.read("/Archive/Nested/readme.txt").unwrap(), b"");

        delete_verified(&volume, "/A long file name.txt", false).unwrap();
        delete_verified(&volume, "/PROJECTS", true).unwrap();
        delete_verified(&volume, "/Archive", true).unwrap();
        assert!(matches!(
            volume.metadata("/PROJECTS"),
            Err(unifat::FsError::NotFound)
        ));
        assert!(matches!(
            volume.metadata("/Archive"),
            Err(unifat::FsError::NotFound)
        ));

        create_empty(&volume, "/Durable.txt").unwrap();
        save_verified(&volume, "/Durable.txt", b"old contents").unwrap();

        // A failed metadata commit leaves the intended directory sector in
        // unifat's cache. Verification through that same cache must not turn
        // the device error into success.
        fail_writes.set(true);
        assert!(matches!(
            save_transactional(&volume, "/Durable.txt", b"new contents"),
            Err(unifat::FsError::Io(_))
        ));
        assert_eq!(volume.read("/Durable.txt").unwrap(), b"old contents");
        assert!(matches!(
            create_directory_verified(&volume, "/Must fail"),
            Err(unifat::FsError::Io(_))
        ));
        assert!(matches!(
            create_empty(&volume, "/Must also fail.txt"),
            Err(unifat::FsError::Io(_))
        ));
    }

    #[test]
    fn probe_rejects_bpb_larger_than_its_container() {
        let geometry = FormatGeometry {
            partition_start: 0,
            partition_sectors: 100_000,
            sectors_per_cluster: 1,
            fat_sectors: 800,
            cluster_count: 98_368,
            volume_serial: 1,
        };

        let mut superfloppy = RamDisk {
            bytes: make_boot_sector(&geometry).to_vec(),
            position: 0,
            fail_writes: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            probe_fat(&mut superfloppy, geometry.partition_sectors - 1),
            Err(ProbeError::Invalid)
        );

        let partition_start = 1u32;
        let partition_sectors = geometry.partition_sectors;
        let mut mbr = vec![0u8; 1024];
        mbr[446 + 4] = 0x0c;
        put_u32(&mut mbr[446 + 8..446 + 12], partition_start);
        put_u32(&mut mbr[446 + 12..446 + 16], partition_sectors);
        mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
        let mut oversized_boot = make_boot_sector(&FormatGeometry {
            partition_start,
            partition_sectors: partition_sectors + 1,
            ..geometry
        });
        put_u32(&mut oversized_boot[32..36], partition_sectors + 1);
        mbr[512..1024].copy_from_slice(&oversized_boot);
        let mut partitioned = RamDisk {
            bytes: mbr,
            position: 0,
            fail_writes: Rc::new(Cell::new(false)),
        };
        assert_eq!(
            probe_fat(&mut partitioned, partition_start + partition_sectors),
            Err(ProbeError::Invalid)
        );
    }

    #[test]
    fn save_errors_are_compact_enough_for_the_status_line() {
        let error = unifat::FsError::Io(unifat::PartitionError::Io(SdStreamError::DeviceWrite(
            "WriteError".into(),
        )));
        let status = alloc::format!("Save failed: {}", save_failure_reason(&error));
        assert_eq!(status, "Save failed: card rejected write");
        assert!(status.chars().count() <= 32);
    }
}
