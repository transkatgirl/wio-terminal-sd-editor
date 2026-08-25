# Wio Terminal SD Explorer and Text Editor

An interactive, `no_std` Rust demo for the Seeed Studio Wio Terminal. It
browses FAT16/FAT32 SD cards, navigates subfolders, creates long-name `.txt`
files, and edits text with a joystick-controlled on-screen keyboard.

When a newer Wio Terminal Battery Chassis with a BQ27441 fuel gauge is
attached, the top-right corner shows its charge level. `CHG` means the battery
is charging; `BAT` means it is attached but not charging. Older chassis models
without the fuel gauge cannot report battery status.

## Controls

### File explorer

- Joystick up/down: move the selection
- Joystick right or click: open a folder or `.txt` file
- Joystick left or top-left: parent folder
- Top-middle: open the **New** menu to create a `.txt` file or folder
- Top-right: open **Actions** for rename, move, delete, or refresh

Directory listings are paged instead of being loaded wholly into RAM. A `*`
beside an entry marks a hidden or system file.

Rename edits the complete file or folder name, including a file extension.
Move opens a folder-only destination picker and never replaces an existing
item. Delete requires confirmation; deleting a folder recursively removes all
of its contents, and Cancel is selected by default.

### File name and text editor

- With the keyboard visible, move the joystick to select a key and click to
  activate it.
- `CASE` changes letter case, `PAGE` cycles letters/symbols, and the final keys
  provide space, backspace, delete, and enter.
- Top-middle hides or shows the keyboard. With it hidden, the joystick moves
  the text cursor; click shows the keyboard again.
- Top-right saves immediately.
- Top-left exits through a Save / Discard / Cancel prompt. Cancel is selected
  by default.

The editor accepts valid UTF-8 text up to 32 KiB and preserves LF or CRLF line
endings. The display font is ASCII-only, so other valid Unicode characters are
shown as `?` but remain unchanged in the file.

## SD card behavior

The demo supports FAT16 and FAT32 in either an MBR partition or a superfloppy
layout. FAT12, exFAT, GPT, and damaged layouts are not mounted.

When a readable card has an unsupported or invalid layout, the device offers
to create one MBR-aligned FAT32 partition. **Formatting erases the entire
card.** Cancel is the default action and formatting requires holding the
top-right button for two seconds. I/O failures do not offer formatting.

Saves rewrite the complete document and read it back before reporting success.
If a card is removed while editing, the document remains in RAM. Saving is
enabled again only when a card with the same capacity, partition offset, and
FAT volume serial is reinserted.

New file stems are limited to 48 on-screen ASCII characters. FAT-reserved
characters and names ending in a space or period are rejected; `.txt` is added
automatically. New folder names use the same 48-character input limit; rename
can preserve and edit longer existing FAT names.

## Build and flash

Install the Cortex-M target and make a release build:

```sh
rustup target add thumbv7em-none-eabihf
cargo build --release
```

The ELF is generated at
`target/thumbv7em-none-eabihf/release/wio-terminal-sd-editor`.

Put the Wio Terminal into bootloader mode by toggling reset twice. With
`cargo-hf2` installed:

```sh
cargo hf2 --release --vid 0x2886 --pid 0x002d
```

Host-side logic tests must override the embedded default target:

```sh
cargo test --target aarch64-apple-darwin --lib
```

Use the appropriate host triple in place of `aarch64-apple-darwin` on other
platforms.

## License

Licensed under either Apache-2.0 or MIT.
