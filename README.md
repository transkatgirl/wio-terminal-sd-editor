# Wio Terminal SD Explorer and Text Editor

An interactive, `no_std` Rust demo for the Seeed Studio Wio Terminal. It
browses FAT16/FAT32 SD cards, navigates subfolders, creates 8.3-named `.txt`
files, and edits text with a joystick-controlled on-screen keyboard. Existing
files with long names are listed under those names and can be opened and
edited; newly created names are limited to FAT short names.

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
beside an entry marks a hidden or system file. The bottom row shows the
card's remaining space: on FAT32 it comes from the volume's FSInfo sector,
so a card formatted elsewhere without a free-cluster count shows no figure
(reformatting the card, in-app or on a PC, restores it); on FAT16 it is
counted from the FAT itself.

Rename edits the complete stored file or folder name, including a file
extension. Move opens a folder-only destination picker and never replaces an
existing item. Renames and moves copy the entry to its new name and then
delete the original (folders recursively), so moving a large file takes time;
a working notice is shown while the operation runs.
A folder move that fails partway removes its partial destination copy, but an
interrupted one (power loss, card removal) can still leave that partial copy
behind. Entries carrying the read-only attribute -- and folders containing
one anywhere inside -- refuse rename, move, and delete, because the device
has no way to clear the attribute. Folder trees nested deeper than 32 levels
are refused. Delete requires confirmation; deleting a folder recursively
removes all of its contents, and Cancel is selected by default.

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
shown as `?` but remain unchanged in the file. A save target that has grown
beyond 32 KiB on another machine is refused rather than overwritten.

## SD card behavior

The demo supports FAT16 and FAT32 in an MBR partition. FAT12, exFAT, GPT,
superfloppy (partition-table-less), FAT32 volumes with fewer than 65,525
clusters (as `mkfs.vfat -F 32` creates on small media), and damaged layouts
are not mounted; superfloppy cards are offered the built-in format, which
writes an MBR. MBR slots whose status byte is neither `0x00` nor `0x80` are
skipped, matching what the mount layer accepts.

When a readable card has an unsupported or invalid layout, the device offers
to create one MBR-aligned FAT32 partition. **Formatting erases the entire
card.** The top-left button cancels, disarming the prompt (top-right re-arms
it), and formatting requires holding the top-right button for two seconds.
I/O failures do not offer formatting.

Saves rewrite the complete document and read it back before reporting success.
Before the document itself is touched, the new contents are staged and
verified as a `~WIO*.TMP` file and the old contents as a `~WIO*.BAK` file;
both are removed after a confirmed save. Because the backup must be a copy on
a rename-less filesystem, an overwrite save needs free space for roughly
twice the document size. If a save is interrupted (power loss,
card removal), those files remain on the card and hold the new and old
contents for recovery on a PC. If a card is removed while editing, the
document remains in RAM. Saving is enabled again only when a card with the
same capacity, partition offset, and FAT volume serial is reinserted.

Created names use FAT 8.3 short names: file stems are limited to 8
characters, `.txt` is added automatically, and folder or rename names allow
at most 8 characters, an optional dot, and a 3-character extension. Names are
stored uppercase; spaces and FAT-reserved characters are rejected. Existing
long names are displayed for reading, but rename starts from the entry's
stored short name, and device-written entries carry a fixed timestamp because
the hardware has no calendar clock.

### Known limitations

The bundled filesystem layer (`embedded-sdmmc` 0.10) imposes limits the
device works around where it can:

- Its delete never frees FAT cluster chains. On FAT32 the device reclaims a
  deleted file's chain itself, all but one cluster per file; deleted folders'
  own directory clusters, and all deletions on FAT16, stay allocated until
  the card is checked on a PC (`chkdsk` / `fsck.fat`).
- Its truncate frees a chain's final cluster without counting it, so the
  FAT32 free-cluster count (the FSInfo sector, which the remaining-space
  display reads) drifts one cluster low per overwrite or delete. The drift
  never overstates free space, and a PC disk check rebuilds the count.
- Deleting or renaming a file that carries a long filename leaves the long
  name's directory entries orphaned; a PC disk check reports and removes
  them.
- It cannot copy attributes or timestamps, so a rename or move produces a
  writable entry with the fixed device timestamp, and hidden or system flags
  are lost; read-only entries are refused outright (see above).
- Cluster numbers read from a corrupt FAT are not bounds-checked against the
  partition, so a damaged filesystem should be repaired on a PC before being
  edited on-device.

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

The included `.vscode/settings.json` sets `rust-analyzer.check.allTargets` to
`false` so rust-analyzer does not report phantom `can't find crate` errors from
checking the host-only test module against the embedded default target; apply
the same rust-analyzer setting in other editors.

## License

Licensed under either Apache-2.0 or MIT.
