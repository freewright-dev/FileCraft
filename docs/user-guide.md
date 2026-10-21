# User guide

FileCraft indexes every file and folder on your fixed drives and finds them as you type,
like [Everything](https://www.voidtools.com/). Start `FileCraft.exe`; there are no command-line options.

## Window

- **Search box** — results update on every keystroke. The toggles on the right are
  **Aa** (match case), **\\** (match the full path instead of the name) and **.\*** (treat the whole
  query as a regular expression). The same options are in the **Search** menu.
- **Results** — Name, Path (containing folder), Size and Date Modified. Click a column header to
  sort; click it again to reverse. Folder sizes are the total size of everything inside them.
  Hidden and system items are shown dimmed.
- **Preview pane** (F2) — shows the selected item: text files with an editor, images, a hex dump
  for binary files, or a folder's contents. Drag its left edge to resize it.
- **Status bar** — number of results, search time, and one indicator per drive:
  `…` waiting, `45%` reading the MFT, `1,234,567` entries found by a folder scan so far,
  `✔` ready, `⟳` refreshing in the background, `✖` failed (hover for details).

## Indexing

On start, each drive's index is loaded from the cache in `%LOCALAPPDATA%\FileCraft\index`,
so the window is searchable in about a second. A drive is then re-indexed in the background when
its cache is older than the configured age (default 60 minutes); **File → Re-index all drives**
(F5) forces it. Results stay searchable while re-indexing runs.

| Mode | When | Speed |
| --- | --- | --- |
| **NTFS MFT** | Running as administrator, NTFS drive | Reads the master file table directly: seconds per drive |
| **Folder scan** | Otherwise (or if the MFT read fails) | Parallel directory walk: ~20 s for 4 million files on an SSD, longer on hard disks |

Use **⚡ Run as admin** in the status bar (or **File → Restart as administrator**) to restart
elevated. A folder scan cannot see into folders your account cannot open; the MFT read sees
everything on the volume.

The index does not follow changes live: files created or deleted after the last index appear on
the next re-index.

## Search syntax

Terms separated by spaces must all match. Matching is case-insensitive unless **Match case** is on.

| Query | Finds |
| --- | --- |
| `foo bar` | names containing `foo` and `bar` |
| `"foo bar"` | names containing the exact text `foo bar` |
| `!foo` | names not containing `foo` |
| `foo\|bar` | names containing `foo` or `bar` |
| `*.rs`, `a?c` | wildcards, matched against the whole name |
| `src\main`, `c:/users/` | a term with `\` or `/` matches the full path |
| `ext:pdf;docx` | files with these extensions |
| `file:` / `folder:` | only files / only folders; `folder:src` = folders containing `src` |
| `size:>100mb` | file size; also `>=`, `<`, `<=`, `size:1mb..10mb`, `size:empty` |
| `dm:today` | modified today; also `yesterday`, `30min`, `24h`, `7d`, `2w`, `1y` |
| `dm:2026-01-31` | modified that day; also `dm:>2026-01-01`, `dm:<2026-01-01`, `dm:2026-01-01..2026-02-01` |
| `regex:^IMG_\d+\.jpg$` | regular expression on the name |
| `path:foo` | match this term against the full path |

Prefixes combine with everything else: `report ext:pdf dm:7d !draft`.

## Reading and editing files

Select a file to preview it. Text files show their first 512 KB.

- **✏ Edit** (or F4) turns the preview into an editor for files up to 8 MB that are UTF-8 text.
  **Ctrl+S** saves. The file's byte-order mark and Windows (`CRLF`) line endings are kept.
- If the file was changed, replaced or deleted on disk after you started editing, saving stops
  and offers **Overwrite** or **Reload from disk**. Overwrite recreates a deleted file.
- Saving writes into the existing file, so its permissions, attributes and hard links are kept.
  A copy of the original is kept in `%LOCALAPPDATA%\FileCraft\backup` until the write finishes;
  if a save fails and the original cannot be restored, the error names that copy.
- Saving fails with "being used by another process" while another program has the file open
  for writing.
- Switching to another result or closing the window with unsaved changes asks whether to save.
- **External editor** (Ctrl+E) opens the file in the editor set in **Settings** (default: the
  `FILECRAFT_EDITOR`, `VISUAL` or `EDITOR` environment variable, else Notepad). For VS Code use `code`.

## Actions

Right-click a result, use the preview pane buttons, or the keyboard:

| Key | Action |
| --- | --- |
| `↑` `↓` `PgUp` `PgDn`, `Ctrl+Home` `Ctrl+End` | Move the selection |
| `Enter` / double-click | Open with the default application |
| `Ctrl+Enter` | Show in Explorer |
| `Alt+Enter` | Properties |
| `F4` | Edit in the preview pane |
| `Ctrl+S` | Save the file being edited |
| `Ctrl+E` | Open in the external editor |
| `Ctrl+Shift+C` | Copy the full path |
| `Ctrl+F` / `Esc` | Focus the search box / clear it |
| `F1` | Search syntax and shortcuts |
| `F2` | Toggle the preview pane |
| `F5` | Re-index all drives |

## Settings

**File → Settings…**

- **External editor** — command line used by Ctrl+E; the file path is appended. The program must
  be an `.exe`, `.com`, `.bat` or `.cmd` on `PATH` or a full path (quote paths with spaces, e.g.
  `"C:\Program Files\Editor\editor.exe" -n`). It runs directly, not through a shell.
- **Drives** — untick a drive to stop indexing and searching it.
- **Re-index on startup when older than** — cache age in minutes (0 re-indexes on every start).
- **Delete cached indexes** — removes the cache files; drives are rebuilt on the next re-index.

Settings, the window size, sort order and search options are remembered between sessions.
