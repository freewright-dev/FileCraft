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
- NTFS master file table indexing, with a parallel folder scan as the fallback. **⚡ Fast NTFS
  indexing** starts a small elevated helper (one UAC prompt) that reads the master file tables for
  the window, which keeps running without administrator rights (FC-18).
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
- CI on Windows (check, `clippy -D warnings`, tests) plus `cargo deny` and `cargo audit`. The
  dependency policy in `deny.toml` checks the Windows graph, keeps advisory checks on, allows
  only reviewed licenses and crates.io sources. A lint policy in `Cargo.toml` requires
  `// SAFETY:` comments on `unsafe` blocks (MIN-03).
- CI also runs the tests in the release profile (overflow checks off) and runs every week, so new
  advisories and yanked crates show up between pull requests. Third-party actions are pinned to
  commit SHAs, and Dependabot proposes updates (MIN-04).
- Quick search bar: a system-wide hotkey (default Alt+Space, configurable in Settings) opens a small
  always-on-top search box that ranks programs and close name matches first. Enter opens the
  result, Ctrl+Enter shows it in its folder, Shift+Enter shows all results in the main window.
- Notification area icon while quick search is on: clicking it shows the window, and its menu
  offers Open FileCraft, Quick search and Exit. Starting FileCraft again shows the running instance.

### Changed

- README: removed the Freewright and Discord links.
- With quick search on (the default), closing the window no longer exits FileCraft: it keeps
  running in the notification area so that the hotkey keeps working. **Exit** (in the File menu or
  the icon's menu) quits, and turning quick search off in Settings restores the old behaviour. The
  first close explains this once and offers to exit instead.

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
- Finishing an index no longer runs its four sort orders side by side. Each sort already uses
  every core, so a large drive briefly ran up to four times as many sort threads as cores and
  held four merge buffers at once, multiplied again when several drives finished together. The
  sorts now run one after another.
- A crafted index cache can no longer make loading run out of memory and crash. Its entry count
  was bounded only by the 4 GiB file size, so it could declare over 100 million entries, needing
  more than 12 GiB while loading. Caches with more than 64 Mi entries (the NTFS reader's limit)
  are now rejected and rebuilt, and the file's bytes are freed before the remaining checks run.
- Date filters such as `dm:2026-01-15` or `dm:>2026-07-01` now start at local midnight of that
  date. They used the UTC offset in effect today, so for a date on the other side of a
  daylight-saving change the day started and ended an hour early or late. `dm:today` also starts
  at the right time on the day the clocks change.
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
- A malformed image that makes the decoder panic no longer stops previews. The panic ended the
  preview thread, so no other item previewed again (and release builds closed FileCraft, losing
  unsaved edits); now the pane shows "Cannot show image" for that file.
- Selecting a cloud or archive placeholder that is recalled when it is opened (the
  "recall on open" attribute some sync and storage tiering providers use) no longer downloads it.
  Only offline and recall-on-access placeholders were skipped before; now these get the same
  "not fully available on this device" message instead of a preview.
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
- Concurrent cache saves for the same drive no longer collide on one `<letter>.tmp` file, which
  made most of them fail under load. Saves use unique, exclusively created and synced staging
  files. A cancelled indexing job no longer replaces the cache, and an older snapshot never
  replaces a newer one from another instance. Leftover staging files are cleaned up (FC-07).
- Saving an inline edit no longer reads, hashes and backs up the whole file on the UI thread
  (FC-13):
  - The save runs on a worker thread, so checking, backing up, writing and flushing no longer
    freeze the window.
  - A file whose length changed since it was loaded is a conflict, found without reading it.
  - Overwrite streams the backup copy instead of holding the file in memory. A file that grew to
    gigabytes can be overwritten without using memory equal to its size.
- Extreme file times, which any user can set, and folder sizes that add up past the maximum no
  longer overflow. Debug builds panicked on the UI thread when such a row was shown; release
  builds could show a wrong date or a folder size of zero (FC-19).
- Deep folder trees index in linear time. Building the full paths for the path sort and summing
  folder sizes both cost time and memory proportional to the depth of every entry. A 32,000-level
  chain took about 4 s and 1.3 GB in release builds; it is now a few milliseconds. The path sort
  now orders folders by their names from the root, compared name by name. This is the same as
  sorting the full path text except where one sibling's name is a prefix of another's (`a` and
  `a b`), where folders sort by their own chain (FC-12).
- Path queries (`\`, `/`, `path:`, *Match path*) no longer rebuild every result's full path from
  the drive root. On a 20,000-level folder chain each keystroke took about 2 s; it now takes
  milliseconds, whatever the sort column (FC-12).
- A quoted term with a `/`, such as `"src/main"`, now matches full paths like the same term
  without quotes. Only unquoted terms had their `/` read as `\`, so quoted ones found nothing.
- `!folder:tmp` and `!file:tmp` now exclude the folders (or files) containing `tmp` and keep
  everything else. The `!` applied only to the text, so `!folder:tmp` meant "folders not
  containing `tmp`" and hid every file.
- A search sorted by name, size or date over several drives no longer copies its whole merged
  result list once more before returning it: about 40 MB of memory traffic per keystroke on a
  5-million-entry index.
- A keystroke no longer starts a thread per drive plus up to 32 more per drive (128 or more on a
  machine with four drives) to filter the index. All drives now share one set of at most as many
  threads as the CPU runs at once, and filtering fewer than 50,000 entries starts none.
- Date filters (`dm:2026-03-29`, `dm:yesterday`, `dm:>date`, `dm:<=date`, `dm:a..b`) now end each
  day at the next date's midnight. They used to add 24 hours to the day's start, so on a
  daylight-saving change day the range ended an hour early or late.
- The searcher no longer keeps an unfiltered result list for every sort order clicked (up to
  eight, about 320 MB on a 5-million-entry index). It keeps the last two, enough to flip a column
  between ascending and descending.
- If the search thread has stopped (after a crash in it), a search now reports "Searching stopped
  working. Restart FileCraft." instead of showing a spinner that never ends.
- Hiding the preview pane (F2 or *View → Preview pane*) while typing in its editor no longer
  leaves the arrow keys, Enter, F4, Ctrl+E and the other result shortcuts switched off until the
  pane was shown again and the editor lost focus.
- Pressing Enter, F4, the arrow keys and the other result shortcuts while typing in a Settings
  field (such as *External editor* or the hotkey) now edits the field. Enter used to open the
  selected result and the key never reached the field.
- When the preview went away while a save was running, finishing the save no longer drops what
  waited for it: closing the window or switching to another result now goes ahead, and the status
  bar reports whether the file was saved.
- With results sorted by name, keeping the selected item selected when new results arrive no
  longer checks every result: it looks only at the results with the same name. Clearing the query
  on a large index used to compare the name of every entry.
- The taskbar icon loaded at startup is now a shared system icon, so its handle is no longer
  leaked for the life of the process.
- A cache stamped with a build time more than five minutes in the future (clock skew, or a
  corrupt cache) no longer blocks every later save. The drive used to be rebuilt on every launch
  and its new index never written back, silently (FC-11).
- Items whose names end in a dot or a space are previewed, edited and saved themselves. Before,
  Win32 path normalisation sent these actions to a sibling (`report.txt.` opened `report.txt`), and
  the folder scan listed a `sub.` folder's twin `sub` or nothing. File-system calls and the folder
  scan now use verbatim `\\?\` paths. Open, Show in folder, Properties and External editor take
  only plain paths, so they are refused for such names and for device names such as `CON` (FC-15).
- The NTFS index lists a file with several hard links under each of them, as the folder scan
  does. It kept only one name per record before, so the two modes found different sets (MIN-04).
- When a save stops because the file differs on disk, the message says whether it was changed,
  replaced by a different file or deleted, and what Overwrite will do. Before, it said only
  "changed or was deleted", so Overwrite could write into a file that had replaced the original
  without saying so (MIN-04).
- Starting FileCraft or turning quick search on no longer freezes the window while Explorer is
  slow to add the notification area icon, for example while the taskbar restarts. The background
  thread now reports that it has started before it adds the icon, instead of after.
- When the quick search background thread ends unexpectedly while the window is hidden, FileCraft
  wakes up to bring the window back and say so. It used to depend on eframe happening to run the
  app's logic for a hidden window, so it could stay unreachable with no tray icon or hotkey.
- The quick search bar hides its window through the message queue instead of with a synchronous
  call made while its state is locked. Had Windows run the bar's UI again during that call,
  FileCraft would have hung waiting for its own lock.
- The quick search bar takes the keyboard focus once it is actually on screen. The hotkey thread
  used to make it the foreground window before the busy UI thread had shown it, so while
  FileCraft was indexing or merging, keystrokes could go to an invisible window.
- Pressing the quick search hotkey twice in quick succession closes the bar again. The hotkey
  decided by whether the window was already visible, which lags behind showing and hiding it, so
  a fast second press could open the bar again instead of closing it (and the other way round).
- The notification area icon's menu opens at the icon when opened with the keyboard (the Menu key
  or Shift+F10), instead of wherever the mouse pointer happened to be, and an item can be chosen by
  releasing the right mouse button over it.
- Restoring the notification area icon after Explorer restarts no longer leaks an icon handle
  each time; the icon is loaded as a shared resource.
- Turning quick search off or exiting releases the hotkey explicitly, before removing the
  notification area icon. It was left to window teardown, after the icon, so if Explorer was slow
  to respond, turning quick search back on could report the hotkey as taken by FileCraft itself.
- Leaving the hotkey field in Settings without changing the hotkey no longer registers it again.
  Each time the field lost focus the hotkey was briefly released and re-registered, so a press at
  that moment was lost. A hotkey that failed to register is still retried.
- Closing Settings while still typing in the hotkey field applies the hotkey typed there. Before,
  only leaving the field did, so Settings kept showing a hotkey that was never registered (and
  was not saved), with no error. Text that is not a hotkey gives way to the hotkey in effect when
  Settings closes, and its error no longer replaces a message saying why quick search is not
  working.
- `Shift+Enter` in the quick search bar no longer discards your saved search options for good. It
  still switches the main window to the default options so that it shows the bar's results, but
  only for the session: your own Match case, Match path and Regex choices are what is remembered,
  unless you change an option yourself afterwards.
- The save conflict check identifies the file by its full 64-bit volume serial and 128-bit file id
  (`FileIdInfo`) and compares its length as well as its content hash. The 64-bit file index used
  before cannot hold ReFS ids (MIN-04).
- `!` and the `file:`, `folder:`, `ext:`, `size:`, `dm:`, `regex:` and `path:` prefixes apply to a
  quoted phrase written after them: `!"old copy"` and `folder:"my docs"` work. They were searched
  for as text before (`!old copy`, `folder:my docs`). Inside quotes everything stays literal (MIN-04).
- Loading a cache checks that its sort orders are sorted exactly as a rebuild would sort them, so
  a tampered cache can no longer show results out of order (MIN-04).
- Indexing fails with a clear error instead of corrupting the index when a drive exceeds the
  index format's limits: about 4 billion entries (the folder scan's id counter used to wrap to the
  root's id) or 4 GiB of file names (offsets used to wrap onto other names) (MIN-04).
- The cache of unfiltered results (an empty query) holds the snapshot it was computed on instead of
  only its address, so a newer snapshot allocated at a reused address can never be shown the old
  results (MIN-04).
- Failures of Open report what ShellExecute returned (for example "no application
  is associated with this kind of file") instead of an unrelated last OS error (MIN-04).
- On a volume whose `$MFT` is fragmented enough that its run list continues in an
  `$ATTRIBUTE_LIST`, the NTFS reader no longer indexes only the records the first runs cover
  while reporting the drive as ready. It reads every piece the attribute list names, from the
  extension records holding them, and checks that the pieces join up and cover the whole table.
  Anything inconsistent falls back to the folder scan (FC-14).
- `cargo deny check` passes again: the lockfile moves `clipboard-win` from the yanked 5.4.1 to
  5.4.3, which the CI supply-chain job rejected (FC-16).

### Security

- External editor launches no longer go through `cmd /C`, so shell syntax in a file name (such as
  `a&calc&.txt` or `%VAR%`) can no longer run commands. The editor is resolved on `PATH` and
  started directly; `.cmd` shims such as VS Code's `code` still work. Quoted editor paths with
  spaces are now accepted (FC-01).
- A rewritten index cache can no longer give an entry a name such as `..\..\Windows\win.ini`, under
  which a harmless-looking row would preview, edit, save or open another file. Loading rejects
  names that are empty, `.`, `..` or contain `\`, `/` or NUL. Show in folder refuses paths with a
  `"`, which could end Explorer's argument and add switches of its own (FC-17).
- An index entry can no longer be named like `notes.txt:payload`. Windows reads such a name as the
  `payload` data stream of the neighbouring file `notes.txt`, so previewing, editing or saving the
  entry acted on a hidden stream of another file. Loading a cache now rejects names containing
  `:`, and the indexers store them as `?`, like other names that are not a usable path component.
- Loading an index cache now applies the same name checks to each entry's lowercased name, which
  search and the selection use. Before, only the displayed name was checked, so a rewritten
  cache could pair a harmless name with a lowercased one such as `..\..\x`.
- Open, Show in folder, Properties and the external editor now also refuse items named like the
  devices `CONIN$`, `CONOUT$` and `CLOCK$`, or `COM¹`–`COM³` and `LPT¹`–`LPT³`, which Windows
  reads as `COM1`–`COM3` and `LPT1`–`LPT3`, and check components separated by `/` as well as
  `\`. Before, such a path was handed to the shell, which acted on a device or a different item.
- An instance running as administrator no longer trusts state that unelevated programs of the same
  user can change (FC-18):
  - It asks before running the external editor command, which comes from saved settings or the
    environment, and shows the program it resolves to. It asks once per command per session.
  - Cache saves, the staging cleanup and save backups refuse to write when `%LOCALAPPDATA%\FileCraft`,
    `index` or `backup` is a junction or symbolic link, which could aim them at a protected folder.
- The external editor is no longer looked up in the current folder. An empty or relative `PATH`
  entry (`a;;b`, `.\bin`) made FileCraft look for the editor in whatever folder it was started
  from, so a program planted there could run instead, and the program it reported could differ
  from the one Windows then started. Such entries are skipped, a relative editor path is refused,
  and the editor is started by the full path it was resolved to.
- When FileCraft runs as administrator, confirming the external editor now runs exactly the
  program the confirmation showed. Before, the command was looked up again by name after you
  confirmed it, so the program that ran was not bound to the one you had checked, and later
  launches of the same command were never checked again. A confirmed command that now resolves to
  another program is asked about again (FC-18).
- Show in folder starts `explorer.exe` from the Windows folder. It ran a bare `explorer`, which
  Windows looks up in FileCraft's own folder first, so an `explorer.exe` placed next to a portable
  `FileCraft.exe` ran instead.
- Fast NTFS indexing no longer runs the whole window as administrator (FC-18). **Run as admin**
  restarted FileCraft elevated, so opening files, editing, saving, backups, cache writes and the
  external editor all ran with administrator rights on state that unelevated programs can change.
  Now only a small helper runs elevated:
  - it reads master file tables and nothing else;
  - it is reached through a single-instance, local-only pipe and cannot be impersonated through it;
  - the window checks everything it sends like a cache file.
  The safeguards above still apply when FileCraft itself is started as administrator.
