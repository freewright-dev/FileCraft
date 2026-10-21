//! Fallback indexer: a parallel directory walk (works without administrator rights).

use crate::index::{ATTR_DIRECTORY, ATTR_REPARSE_POINT, Source, VolumeBuilder, VolumeIndex, thread_count};
use crossbeam_channel::{RecvTimeoutError, Sender, bounded, unbounded};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Most entries sent to the collector at once; big folders are streamed in several batches.
const BATCH: usize = if cfg!(test) { 8 } else { 4096 };
/// Batches waiting for the collector before workers wait for it. Only workers send and only the
/// collector receives, so a full channel slows the workers down but cannot deadlock.
const QUEUED_BATCHES: usize = 64;
/// Folders waiting in the shared queue before a worker keeps the ones it finds on its own stack
/// (walked depth-first), which bounds the shared queue without ever blocking a worker on it.
const SHARED_QUEUE_LIMIT: usize = 4096;

/// State every worker shares.
struct Shared<'a> {
    next_id: AtomicU32,
    /// Folders found but not finished, wherever they wait.
    pending: AtomicUsize,
    skipped: &'a AtomicUsize,
    cancel: &'a AtomicBool,
}

struct Raw {
    idx: u32,
    parent: u32,
    name: String,
    attrs: u32,
    size: u64,
    mtime: i64,
}

/// Indexes everything under `root` (normally `X:\`). Ids are handed out by an atomic counter so
/// workers can enqueue subdirectories with their final id while a single collector fills the table.
/// Folders and entries that cannot be read are left out and counted in `skipped`.
pub fn walk_volume(
    letter: char,
    root: &Path,
    progress: &AtomicUsize,
    skipped: &AtomicUsize,
    cancel: &AtomicBool,
) -> anyhow::Result<VolumeIndex> {
    let start = Instant::now();
    std::fs::read_dir(root)?; // fail early when the drive is not readable

    let shared = Shared { next_id: AtomicU32::new(1), pending: AtomicUsize::new(1), skipped, cancel };
    let (work_tx, work_rx) = unbounded::<(PathBuf, u32)>();
    let (out_tx, out_rx) = bounded::<Vec<Raw>>(QUEUED_BATCHES);
    work_tx.send((root.to_path_buf(), 0))?;

    let threads = (thread_count() / 2).max(2);
    let mut builder = VolumeBuilder::new(letter);

    std::thread::scope(|s| {
        for _ in 0..threads {
            let work_tx = work_tx.clone();
            let work_rx = work_rx.clone();
            let out_tx = out_tx.clone();
            let shared = &shared;
            s.spawn(move || {
                let mut local = Vec::new();
                loop {
                    let (dir, dir_id) = match local.pop() {
                        Some(job) => job,
                        None => match work_rx.recv_timeout(Duration::from_millis(5)) {
                            Ok(job) => job,
                            Err(RecvTimeoutError::Timeout) => {
                                if shared.pending.load(Ordering::Acquire) == 0 || cancel.load(Ordering::Relaxed) {
                                    break;
                                }
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        },
                    };
                    if !cancel.load(Ordering::Relaxed) {
                        read_one_dir(&dir, dir_id, shared, &mut local, &work_tx, &out_tx);
                    }
                    shared.pending.fetch_sub(1, Ordering::AcqRel);
                }
            });
        }
        drop(out_tx);

        for batch in out_rx.iter() {
            for raw in batch {
                builder.set(raw.idx, &raw.name, raw.parent, raw.attrs, raw.size, raw.mtime);
            }
            progress.store(builder.len(), Ordering::Relaxed);
        }
    });

    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }
    Ok(builder.finish(Source::Walk, start.elapsed()))
}

/// Lists one folder, streaming its entries to the collector in batches of at most [`BATCH`] and
/// queueing its subfolders. Stops part-way when the walk is cancelled.
fn read_one_dir(
    dir: &Path,
    dir_id: u32,
    shared: &Shared,
    local: &mut Vec<(PathBuf, u32)>,
    work_tx: &Sender<(PathBuf, u32)>,
    out_tx: &Sender<Vec<Raw>>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        // The folder itself is indexed; its contents are not.
        shared.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut batch = Vec::new();
    for entry in rd {
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else {
            shared.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // On Windows this metadata comes from the directory listing itself (no extra syscall).
        let Ok(meta) = entry.metadata() else {
            shared.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (attrs, size, mtime) = attributes(&meta);
        let idx = shared.next_id.fetch_add(1, Ordering::Relaxed);
        if attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE_POINT == 0 {
            shared.pending.fetch_add(1, Ordering::AcqRel);
            let job = (entry.path(), idx);
            if work_tx.len() < SHARED_QUEUE_LIMIT {
                let _ = work_tx.send(job);
            } else {
                local.push(job);
            }
        }
        batch.push(Raw { idx, parent: dir_id, name, attrs, size, mtime });
        if batch.len() == BATCH {
            let _ = out_tx.send(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        let _ = out_tx.send(batch);
    }
}

#[cfg(windows)]
fn attributes(meta: &std::fs::Metadata) -> (u32, u64, i64) {
    use std::os::windows::fs::MetadataExt;
    let attrs = meta.file_attributes();
    let size = if attrs & ATTR_DIRECTORY != 0 { 0 } else { meta.file_size() };
    (attrs, size, meta.last_write_time() as i64)
}

#[cfg(not(windows))]
fn attributes(meta: &std::fs::Metadata) -> (u32, u64, i64) {
    let attrs = if meta.is_dir() { ATTR_DIRECTORY } else { 0 };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| (d.as_nanos() / 100) as i64 + 116_444_736_000_000_000);
    (attrs, if meta.is_dir() { 0 } else { meta.len() }, mtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_a_directory_tree() {
        let root = std::env::temp_dir().join(format!("filecraft-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::write(root.join("a").join("one.txt"), "hello").unwrap();
        std::fs::write(root.join("a").join("b").join("two.rs"), "fn main() {}").unwrap();

        let skipped = AtomicUsize::new(0);
        let v = walk_volume('T', &root, &AtomicUsize::new(0), &skipped, &AtomicBool::new(false)).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(v.len(), 4);
        assert_eq!(skipped.load(Ordering::Relaxed), 0);
        let two = v.by_name.iter().copied().find(|&i| v.name(i) == "two.rs").unwrap();
        assert_eq!(v.full_path(two), r"T:\a\b\two.rs");
        let a = v.by_name.iter().copied().find(|&i| v.name(i) == "a").unwrap();
        assert_eq!(v.entry(a).size, 17);
    }

    /// Runs `read_one_dir` alone and returns the batches it sent.
    fn read_alone(dir: &Path, skipped: &AtomicUsize, cancel: &AtomicBool) -> Vec<Vec<Raw>> {
        let shared = Shared { next_id: AtomicU32::new(1), pending: AtomicUsize::new(1), skipped, cancel };
        let (work_tx, _work_rx) = unbounded();
        let (out_tx, out_rx) = unbounded();
        read_one_dir(dir, 0, &shared, &mut Vec::new(), &work_tx, &out_tx);
        drop(out_tx);
        out_rx.iter().collect()
    }

    /// MIN-02: unreadable folders are counted instead of silently missing.
    #[test]
    fn unreadable_folders_are_counted() {
        let skipped = AtomicUsize::new(0);
        let missing = std::env::temp_dir().join(format!("filecraft-walk-missing-{}", std::process::id()));
        assert!(read_alone(&missing, &skipped, &AtomicBool::new(false)).is_empty());
        assert_eq!(skipped.load(Ordering::Relaxed), 1);
    }

    /// MIN-01: a big folder reaches the collector in bounded batches, and cancellation stops the
    /// listing part-way instead of after the whole folder.
    #[test]
    fn big_folders_are_streamed_and_cancellable() {
        let dir = std::env::temp_dir().join(format!("filecraft-walk-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 * BATCH + 1 {
            std::fs::write(dir.join(format!("{i}.txt")), "").unwrap();
        }
        let batches = read_alone(&dir, &AtomicUsize::new(0), &AtomicBool::new(false));
        let cancelled = read_alone(&dir, &AtomicUsize::new(0), &AtomicBool::new(true));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|b| b.len() <= BATCH));
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 3 * BATCH + 1);
        assert!(cancelled.is_empty());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::index::{Index, SortKey};
    use crate::search::{Query, SearchOptions, Sort, search};
    use std::sync::Arc;

    /// `FILECRAFT_BENCH_ROOT=D:\ cargo test --release bench_real_drive -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real_drive() {
        let root = std::env::var("FILECRAFT_BENCH_ROOT").unwrap_or_else(|_| r"D:\".into());
        let letter = root.chars().next().unwrap().to_ascii_uppercase();
        let t = Instant::now();
        let v = walk_volume(letter, Path::new(&root), &AtomicUsize::new(0), &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        eprintln!("walk: {} entries in {:.2?} (build incl. sorting {:.2?})", v.len(), t.elapsed(), v.build_time);

        let path = std::env::temp_dir().join("filecraft-bench.idx");
        let t = Instant::now();
        v.save(&path).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        eprintln!("cache save: {:.2?} ({} MB)", t.elapsed(), size / 1_000_000);
        let t = Instant::now();
        let v = crate::index::VolumeIndex::load(&path, letter).unwrap();
        eprintln!("cache load: {:.2?}", t.elapsed());
        let _ = std::fs::remove_file(&path);

        let index = Index::default().with_volume(Arc::new(v));
        for (q, key) in [
            ("", SortKey::Name),
            ("a", SortKey::Name),
            ("main", SortKey::Name),
            ("*.rs", SortKey::Size),
            ("ext:png;jpg size:>1mb", SortKey::Modified),
            (r"github\ readme", SortKey::Path),
            (r"regex:^[0-9]+\.txt$", SortKey::Name),
        ] {
            let query = Query::parse(q, SearchOptions::default()).unwrap();
            let t = Instant::now();
            let hits = search(&index, &query, Sort { key, descending: false }, &|| false).unwrap();
            eprintln!("search {q:<26} {:>9} hits in {:.1?}", hits.len(), t.elapsed());
        }
    }
}
