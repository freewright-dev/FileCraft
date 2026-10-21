# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- FileCraft: a Windows desktop app (egui) modelled on Everything, with one search box over
  every fixed drive.
- App icon (an orange folder with a magnifying glass), embedded in the exe and shown in the window and taskbar.
- Hero screenshot in `docs/media/`.
- NTFS master file table indexing when running as administrator, with a parallel folder scan as
  the fallback, and a **Run as admin** button to restart elevated.
- Per-drive index cache in `%LOCALAPPDATA%\FileCraft\index`, loaded at startup and refreshed
  in the background when older than a configurable age (default 60 minutes); F5 re-indexes.
- Everything-style query language: AND terms, `"phrases"`, `!negation`, `a|b`, wildcards, path
  terms, `ext:`, `file:`, `folder:`, `size:`, `dm:`, `regex:` and `path:`, plus Match case,
  Match path and Regex toggles.
- Results table sortable by name, path, size (folders show their total size) and date, with
  precomputed sort orders, parallel filtering and a cross-drive merge.
- Preview pane for text, images, binary (hex) and folders, with an inline editor that keeps the
  BOM and CRLF line endings, detects changes made on disk, and asks before discarding edits.
- Open, Show in folder, Properties, Copy path and name, and External editor (`FILECRAFT_EDITOR`,
  `VISUAL` or `EDITOR`, else Notepad) from the context menu, preview pane and keyboard shortcuts.
- Settings for the editor command, which drives to index and cache age; window size, sort and
  search options persist between sessions.
- User guide, architecture notes and an opt-in benchmark (`FILECRAFT_BENCH_ROOT`).

### Fixed

- Choosing *Edit in preview pane* or pressing F4 again while already editing no longer resets the
  buffer and silently discards unsaved changes (FC-03).
- Saving an inline edit no longer recreates a file that was deleted on disk without asking, and
  detects a file that was replaced or rewritten even when its modification time was kept. The
  file is checked and written through one handle, so the path cannot be swapped in between, and
  a copy of the original is kept until the write completes, so a failed save no longer leaves
  it truncated (FC-04).
- A corrupt index cache with an out-of-range build timestamp no longer crashes FileCraft at
  startup; it is ignored and rebuilt. Loading also rejects caches for another drive, files over
  4 GiB, invalid name ranges, parent loops, incomplete or duplicated sort orders and trailing
  data (FC-02).
- Editing a file longer than its preview no longer reads it on the UI thread without a limit.
  The whole file is read on the preview thread, never more than 8 MB, even if it grew after it
  was previewed, and only if it is still a regular file available locally (FC-05).
- Files nested more than 512 folders deep now get their real full path. Before, the top folders
  were dropped and the rest attached to the drive root, so open, edit and save could act on a
  different file. Path sorting and folder sizes use the full depth too, the NTFS index no longer
  skips such files, and caches whose entries do not all reach the root are rejected (FC-06).
- Image previews work on local drives. They used a malformed `file://D:\…` URI that egui's loader
  turned into a network path. Images are now read and decoded on the preview thread within byte,
  size and memory limits, show only their first frame, are scaled down to fit the pane, and their
  texture is freed with the preview. egui's whole-file image loaders are no longer installed (FC-08).
- Small correctness fixes (MIN-02):
  - Dates such as `dm:2026-02-31` are rejected instead of rolling over into the next month.
  - A file with mixed `CRLF` and `LF` line endings is no longer converted silently. The editor
    says which ending saving will use, and it is the more common one rather than CRLF whenever
    any line had it.
  - A drive whose indexing thread cannot start shows as failed instead of waiting forever.
  - The drive indicator's hover text reports items a folder scan could not read and cache-save
    errors, which were discarded before.
- The NTFS reader rejects invalid `$MFT` data runs (a negative LCN read as `u64::MAX`, overflowing
  or zero-length runs, runs past the end of the volume) and inconsistent volume geometry, and caps
  the record count at 64 Mi. It falls back to the folder scan instead of reading wrong offsets or
  panicking. Records shorter than the fixed header are skipped instead of read out of bounds (FC-10).
- Out-of-range numbers in queries, such as `size:9223372036854775807` or `dm:9223372036854775807y`,
  are reported as errors instead of panicking the search worker (debug builds) or wrapping into
  wrong filters (release builds). A relative date reaching back before 1601 matches every date.
  The saved re-index age is clamped to the settings range of one week (FC-09).
- Previewing a folder with millions of entries no longer collects and sorts all of them to show
  1,000. Only the first 1,000 in display order are kept while the rest are counted, and the
  listing stops as soon as another item is selected. The folder scan streams large folders in
  bounded batches, so a slow collector holds the workers back, and it can be cancelled part-way
  through a folder (MIN-01).

### Security

- External editor launches no longer go through `cmd /C`, so shell syntax in a file name (such as
  `a&calc&.txt` or `%VAR%`) can no longer run commands. The editor is resolved on `PATH` and
  started directly; `.cmd` shims such as VS Code's `code` still work. Quoted editor paths with
  spaces are now accepted (FC-01).
