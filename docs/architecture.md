# Architecture

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Window setup (`eframe`) and app icon |
| `src/app.rs` | The UI: search box, results table, preview/editor, status bar, settings, keys |
| `src/index.rs` | `VolumeIndex` data model, builder, sort orders, folder sizes, disk cache |
| `src/mft.rs` | NTFS master file table reader (administrator) |
| `src/walk.rs` | Parallel directory walk (fallback, no admin) |
| `src/indexer.rs` | Drive discovery and background (re)indexing per drive |
| `src/search.rs` | Query language, parallel filtering, cross-drive merge, background searcher |
| `src/preview.rs` | Preview content (bounded image decoding) and inline editing (encoding, line endings, conflicts) |
| `src/actions.rs` | Shell integration: open, reveal, properties, external editor, elevation |
| `src/util.rs` | FILETIME and size formatting, local-time helpers |

## Index layout

Each drive is one immutable `VolumeIndex`, shared through `Arc` and swapped atomically when a
re-index completes. Entries are fixed-size structs (parent id, attributes, size, FILETIME,
offsets into two string arenas: original names and lowercased names). Entry 0 is the drive root;
full paths are rebuilt on demand by walking parent ids, so no path strings are stored.

Four sort orders (`by_name`, `by_path`, `by_size`, `by_date`) are precomputed with a parallel
merge sort when the index is built. Searching walks the order for the requested column and keeps
matching ids, so results come out already sorted and no per-query sort is needed. The path order
sorts by (rank of the parent folder's full path, name).

Folder sizes are the sum of all files below them, propagated up the parent chain after building.

## Indexing

- **MFT** (`mft.rs`) — opens `\\.\X:`, gets the geometry with `FSCTL_GET_NTFS_VOLUME_DATA`, decodes
  the data runs of `$MFT` from record 0, then reads the table in 4 MB chunks. For each record it
  applies the update-sequence fixups (rejecting torn records) and parses `$STANDARD_INFORMATION`
  (mtime, attributes), `$FILE_NAME` (parent, name; Win32 names are preferred over DOS 8.3 names)
  and the unnamed `$DATA` attribute (size). Extension records are merged into their base record.
  Only records reachable from the root (record 5) through valid parents are kept, which drops the
  NTFS metadata files and anything under `$Extend`. Bad input fails closed and falls back to the
  walk:
  - geometry that is incomplete or inconsistent with the volume size;
  - more than 64 Mi records;
  - a data run that is empty, has a negative or overflowing LCN, or ends past the volume.

  Records shorter than the fixed header are skipped.
- **Walk** (`walk.rs`) — worker threads pull directories from a shared queue. Every entry gets its
  final id from an atomic counter, so subdirectories are queued with their id and a single
  collector thread fills the table without any path lookups. Junctions and symlinks are listed but
  not followed. On Windows the metadata comes from the directory listing itself. Entries reach
  the collector in batches of at most 4,096 through a bounded channel. Only workers send and only
  the collector receives, so a slow collector holds the workers back without any risk of
  deadlock. When the shared queue holds 4,096 folders, a worker keeps the folders it finds on its
  own stack and walks them depth-first. Cancellation is checked between entries, not only
  between folders.
- **Indexer** (`indexer.rs`) — one thread per drive: load the cache and publish it, then rebuild
  if the cache is missing or too old (MFT when elevated and NTFS, else walk, with walk as the
  fallback when the MFT read fails), save the cache and publish again. Each drive has its own
  cancel flag. Progress is exposed through atomics that the status bar reads.

## Cache format

`%LOCALAPPDATA%\FileCraft\index\<letter>.idx`, little-endian: magic and version, drive letter,
source, build time, both name arenas, the entry table and the four sort orders. Files are written
to a temporary name and renamed into place. Loading reads at most 4 GiB and checks everything
before the index is used:
- the drive letter matches the drive being loaded;
- the source byte and build timestamp are valid;
- every name range lies inside its arena on character boundaries;
- parent ids are in range, entry 0 is a root folder, and every entry with a parent reaches
  the root through its parent links (no loops, no unused slot along the way), so full paths are
  always rebuilt completely, at any depth;
- each sort order lists every entry with a parent exactly once;
- no bytes are left over.

A cache that fails any check is ignored and rebuilt; a malformed cache never causes a panic.

## Searching

`Query::parse` turns the text into clauses (AND), each with alternatives (OR, from `a|b`) and an
optional negation. A clause is a substring test (`memchr::memmem` on the lowercased name arena),
a wildcard (`globset`), a regex, an extension, type, size or date test. Full paths are only built
for path terms, once per entry, in a per-thread scratch buffer.

Each drive is filtered on its own thread, and drives with more than 50,000 entries are split
across worker threads by chunks of the sort order. Per-drive results are then merged pairwise in
parallel rounds by the sort key (path order just concatenates drives in letter order).

The `Searcher` runs on a background thread. Every keystroke submits a request with a new
generation number; running searches check it periodically and abandon stale work, and only the
newest result is shown. Each response carries the `Index` snapshot its hits refer to, so the UI
never mixes ids from different snapshots. The unfiltered list for each sort order is cached per
snapshot, so clearing the query is instant.

## UI

`egui`/`eframe` (OpenGL backend). The results table (`egui_extras::TableBuilder`) only lays out
visible rows, so millions of results scroll smoothly. Selection is remembered by path and restored
when new results or a new index arrive. Release builds use the Windows GUI subsystem (no console).
