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

### Security

- External editor launches no longer go through `cmd /C`, so shell syntax in a file name (such as
  `a&calc&.txt` or `%VAR%`) can no longer run commands. The editor is resolved on `PATH` and
  started directly; `.cmd` shims such as VS Code's `code` still work. Quoted editor paths with
  spaces are now accepted (FC-01).
