# Architecture

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Window setup (`eframe`) and app icon; dispatches `--mft-helper` to the helper |
| `src/app.rs` | The UI: search box, results table, preview/editor, status bar, settings, keys |
| `src/index.rs` | `VolumeIndex` data model, builder, sort orders, folder sizes, disk cache |
| `src/mft.rs` | NTFS master file table reader (administrator) |
| `src/mft_helper.rs` | The elevated helper process that runs `mft.rs` for the unelevated window, and its pipe protocol |
| `src/walk.rs` | Parallel directory walk (fallback, no admin) |
| `src/indexer.rs` | Drive discovery and background (re)indexing per drive |
| `src/search.rs` | Query language, parallel filtering, cross-drive merge, background searcher |
| `src/quick_bar.rs` | The quick search bar: its window, its own searcher, launcher-style ranking of the hits |
| `src/hotkey.rs` | Hotkey parsing; the background thread with the global hotkey and the tray icon |
| `src/preview.rs` | Preview content (bounded image decoding) and inline editing (encoding, line endings, conflicts, save backups and their cleanup) |
| `src/actions.rs` | Shell integration: open, reveal and properties (run on short-lived worker threads with COM initialised), external editor |
| `src/util.rs` | FILETIME and size formatting, local-time helpers, verbatim file-system paths (`fs_path`) and detection of names Win32 would rewrite (`win32_alias`) |

## Index layout

Each drive is one immutable `VolumeIndex`, shared through `Arc` and swapped atomically when a
re-index completes. Entries are fixed-size structs (parent id, attributes, size, FILETIME,
offsets into two string arenas: original names and lowercased names). Entry 0 is the drive root;
full paths are rebuilt on demand by walking parent ids, so no path strings are stored.

Four sort orders (`by_name`, `by_path`, `by_size`, `by_date`) are precomputed with a parallel
merge sort when the index is built, one after another so a build never runs more sort threads
or merge buffers than one sort needs. Searching walks the order for the requested column and keeps
matching ids, so results come out already sorted and no per-query sort is needed. The path order
sorts by (rank of the parent folder, name). Folder ranks follow the chain of names from the root,
compared name by name (a folder before its subfolders). They are computed without building paths,
so the cost stays linear in depth; this matches full-path text order except where one sibling's
name is a prefix of another's (`a` and `a b`).

Folder sizes are the sum of all files below them, propagated up the parent chain after building.

## Indexing

- **MFT** (`mft.rs`) — opens `\\.\X:`, gets the geometry with `FSCTL_GET_NTFS_VOLUME_DATA`, decodes
  the data runs of `$MFT` from record 0, then reads the table in 4 MB chunks. Records are numbered
  by their byte position in `$MFT`, so a record split between two runs (clusters smaller than
  records) is put together from both. For each record it
  applies the update-sequence fixups (rejecting torn records) and parses `$STANDARD_INFORMATION`
  (mtime, attributes), `$FILE_NAME` (parent, name; Win32 names are preferred over DOS 8.3 names)
  and the unnamed `$DATA` attribute (size). Extension records are merged into their base record.
  Only records reachable from the root (record 5) through valid parents are kept, which drops the
  NTFS metadata files and anything under `$Extend`. A parent reference is valid only when its
  sequence number matches the parent record's (the volume is read live, so a record can be reused
  meanwhile). Bad input fails closed and falls back to the
  walk:
  - geometry that is incomplete or inconsistent with the volume size;
  - more than 64 Mi records, or record state (about 56 bytes per record) that cannot be allocated;
  - a data run that is empty, has a negative or overflowing LCN, or ends past the volume, or a
    sparse run longer than the volume;
  - runs that hold fewer records than `MftValidDataLength` implies.

  When `$MFT` is fragmented enough, record 0 holds only the first piece of its run list, and an
  `$ATTRIBUTE_LIST` (resident or not) names every piece by starting VCN and the extension record
  holding it. A stored list is read only up to its declared size (at most 16 MB). Those records
  are read through the runs decoded so far. A piece that is missing,
  starts at the wrong VCN (a gap or overlap), or lies in a record those runs do not reach also
  fails closed.

  Records shorter than the fixed header are skipped.
- **Walk** (`walk.rs`) — starts from the verbatim root `\\?\X:\`, so folders whose names end in a
  dot or a space are listed themselves (Win32 strips those characters from plain paths). Worker
  threads pull directories from a shared queue. Every entry gets its
  final id from an atomic counter, so subdirectories are queued with their id and a single
  collector thread fills the table without any path lookups. Junctions, symlinks and mount points
  are listed but not followed; they are counted for the status bar's hover text. A name that is not
  valid Unicode is stored as `?`, counted as skipped and, for a folder, not entered. On Windows the
  metadata comes from the directory listing itself. Entries reach
  the collector in batches of at most 4,096 through a bounded channel. Only workers send and only
  the collector receives, so a slow collector holds the workers back without any risk of
  deadlock. When the shared queue holds 4,096 folders, a worker keeps the folders it finds on its
  own stack and walks them depth-first. Cancellation is checked between entries, not only
  between folders.
- **Indexer** (`indexer.rs`) — one thread per drive: load the cache and publish it, then rebuild
  if the cache is missing or too old (MFT for NTFS drives when the helper runs or the window
  itself is elevated, else walk, with walk as the fallback when the MFT read fails or panics),
  save the cache and publish again. Each drive has its own cancel flag. Progress is exposed through
  atomics that the status bar reads.
- **NTFS helper** (`mft_helper.rs`) — the window never needs administrator rights.
  - **Start.** *Fast NTFS indexing* creates a named pipe with a random name, single instance
    (`FILE_FLAG_FIRST_PIPE_INSTANCE`) and local only, then starts `FileCraft.exe --mft-helper
    <pipe>` with the `runas` verb.
  - **Connect.** The helper accepts only FileCraft's pipe names. It opens the pipe with
    `SECURITY_IDENTIFICATION`, so whoever serves the pipe cannot impersonate its elevated token.
    The window checks the client's process id (`GetNamedPipeClientProcessId`) against the
    helper it started; any other client is disconnected and the window keeps waiting.
  - **Serve.** It answers one request at a time ("index drive X") with progress messages and
    the index in the cache format. It writes no files, reads no settings, starts nothing, and
    exits when the pipe closes. A progress message that cannot be sent cancels the read in
    progress.
  - **Check.** The window parses what it receives with `VolumeIndex::from_bytes`, which applies
    every check a cache file gets. NTFS drives are therefore read one after another. A request
    that hears nothing from the helper for two minutes (progress arrives every 150 ms) is
    aborted with `CancelIoEx` and the helper is no longer used.

## Cache format

`%LOCALAPPDATA%\FileCraft\index\<letter>.idx`, little-endian: magic and version, drive letter,
source, build time, both name arenas, the entry table and the four sort orders.

Each save writes and syncs its own staging file, `<letter>.<pid>-<n>.tmp`, created exclusively so
concurrent saves never share one, and renames it into place, so readers always see a whole
snapshot. The rename is skipped in two cases: the indexing job was superseded (checked under a
process-wide lock, together with the rename), or the cache already holds a snapshot built later,
for example by another FileCraft instance. The newest snapshot wins. Staging files more than an
hour old are leftovers from a crash and are deleted.

Loading reads at most 4 GiB and checks everything
before the index is used:
- the drive letter matches the drive being loaded;
- the source byte and build timestamp are valid;
- the cache holds at most 64 Mi entries (the NTFS reader's record limit), so a crafted count
  cannot make loading allocate many times the file's size;
- every name range lies inside its arena on character boundaries, and every linked entry's name
  and lowercased name are one path component (not empty, `.` or `..`, no `\`, `/`, `:` or NUL;
  the NTFS reader and the folder scan store `?` for names that are not);
- parent ids are in range, entry 0 is a root folder, and every entry with a parent reaches
  the root through its parent links (no loops, no unused slot along the way), so full paths are
  always rebuilt completely, at any depth;
- each sort order lists every entry with a parent exactly once, and is sorted exactly as a
  rebuild would sort it (the path check recomputes the folder ranks, which sorts only the folders);
- no bytes are left over.

The file's bytes are freed before the tree and order checks, which build tables of their own.
A cache that fails any check is ignored and rebuilt; a malformed cache never causes a panic.

## Searching

`Query::parse` turns the text into clauses (AND), each with alternatives (OR, from `a|b`) and an
optional negation. A clause is a substring test (`memchr::memmem` on the lowercased name arena),
a wildcard (`globset`), a regex, an extension, type, size or date test. Full paths are only built
for path terms, once per entry, in a per-thread scratch buffer. The buffer keeps the folder chain
of the last path and only adds or drops the names that differ, so a query with path terms is
filtered in path order (where consecutive paths share most of their folders) and its hits are
then taken in the requested order. That keeps path queries linear however deep the folders.

Queries over more than 50,000 entries in all are cut into parts of each drive's sort order, and
one set of worker threads (as many as the CPU runs at once, at most 32) works through the parts
of every drive; smaller queries are filtered on the searcher's own thread. Per-drive results are then
merged pairwise in parallel rounds by the sort key (path order just concatenates drives in letter
order).

The `Searcher` runs on a background thread. Every keystroke submits a request with a new
generation number; running searches check it periodically and abandon stale work, and only the
newest result is shown. Each response carries the `Index` snapshot its hits refer to, so the UI
never mixes ids from different snapshots. The unfiltered lists of the last two sort orders used
are cached per snapshot, so clearing the query is instant.

## UI

`egui`/`eframe` (OpenGL backend). The results table (`egui_extras::TableBuilder`) only lays out
visible rows, so millions of results scroll smoothly. Selection is remembered by path and restored
when new results or a new index arrive. Release builds use the Windows GUI subsystem (no console).

## Quick search and running in the background

While the main window is hidden, `eframe` runs no UI for it, only `App::logic`, and only when a
repaint is requested. So everything that has to continue then (closing, the index snapshots, polling
the background thread) lives in `logic`, and these two parts work without the window:

- **Background thread** (`hotkey.rs`) — owns a hidden top-level window with the `RegisterHotKey`
  hotkey and the tray icon, and runs its message loop. On the hotkey it places the bar and queues
  showing it, so the bar appears without waiting for the throttled repaint of a hidden main window.
  The bar then takes the foreground from its own thread once it is visible: the process that
  receives a hotkey may do that, but a window that is not shown yet cannot take keystrokes. The
  window class also identifies a running instance, which a second start asks to show its window
  before exiting.
- **Bar** (`quick_bar.rs`) — a *deferred* viewport, which renders on its own; an immediate one only
  renders inside its parent's pass, which does not run for a hidden window. Its state is therefore
  behind a mutex, and it has its own `Searcher`. The main window passes it each new index snapshot.
  Showing and hiding use Win32 calls rather than viewport commands, so that winit's idea of the
  window stays consistent. Hiding happens on the bar's own thread (the hotkey thread only signals),
  because only then does Windows return the foreground to the previous window. It is queued with
  `ShowWindowAsync` all the same, because the bar holds its state's lock while its UI runs.

Calls the hotkey thread makes on the bar are asynchronous (`ShowWindowAsync`, `SWP_ASYNCWINDOWPOS`),
because the main thread owns that window and a synchronous call would wait for it. For the same
reason, dropping `Background` waits for the thread only briefly: the main thread handles no messages
meanwhile. Since the window's visibility lags behind these calls, the hotkey toggles whether the bar
is meant to be open, kept in `BarSignals`, instead of reading it. `WM_CLOSE` to the hidden window
comes from outside (`taskkill` sends it to every top-level window), so it means "exit" and goes
through the usual unsaved-edits question; the app stops the thread with a private message instead.

Ranking runs on the bar searcher's thread. It looks at no more than the first 500,000 hits and keeps
the best few, comparing how the name matches the first plain search term or quoted phrase, then
programs and shortcuts, visibility, name length and depth.
