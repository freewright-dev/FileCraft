//! Fallback indexer: a parallel directory walk (works without administrator rights).

use crate::index::{ATTR_DIRECTORY, ATTR_REPARSE_POINT, Source, VolumeBuilder, VolumeIndex, thread_count};
use crossbeam_channel::{RecvTimeoutError, unbounded};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

    let next_id = AtomicU32::new(1);
    let pending = AtomicUsize::new(1);
    let (work_tx, work_rx) = unbounded::<(PathBuf, u32)>();
    let (out_tx, out_rx) = unbounded::<Vec<Raw>>();
    work_tx.send((root.to_path_buf(), 0))?;

    let threads = (thread_count() / 2).max(2);
    let mut builder = VolumeBuilder::new(letter);

    std::thread::scope(|s| {
        for _ in 0..threads {
            let work_tx = work_tx.clone();
            let work_rx = work_rx.clone();
            let out_tx = out_tx.clone();
            let (next_id, pending) = (&next_id, &pending);
            s.spawn(move || {
                loop {
                    let (dir, dir_id) = match work_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(job) => job,
                        Err(RecvTimeoutError::Timeout) => {
                            if pending.load(Ordering::Acquire) == 0 || cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    if !cancel.load(Ordering::Relaxed) {
                        let batch = read_one_dir(&dir, dir_id, next_id, pending, skipped, &work_tx);
                        if !batch.is_empty() {
                            let _ = out_tx.send(batch);
                        }
                    }
                    pending.fetch_sub(1, Ordering::AcqRel);
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

fn read_one_dir(
    dir: &Path,
    dir_id: u32,
    next_id: &AtomicU32,
    pending: &AtomicUsize,
    skipped: &AtomicUsize,
    work_tx: &crossbeam_channel::Sender<(PathBuf, u32)>,
) -> Vec<Raw> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        // The folder itself is indexed; its contents are not.
        skipped.fetch_add(1, Ordering::Relaxed);
        return Vec::new();
    };
    let mut batch = Vec::new();
    for entry in rd {
        let Ok(entry) = entry else {
            skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // On Windows this metadata comes from the directory listing itself (no extra syscall).
        let Ok(meta) = entry.metadata() else {
            skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (attrs, size, mtime) = attributes(&meta);
        let idx = next_id.fetch_add(1, Ordering::Relaxed);
        if attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE_POINT == 0 {
            pending.fetch_add(1, Ordering::AcqRel);
            let _ = work_tx.send((entry.path(), idx));
        }
        batch.push(Raw { idx, parent: dir_id, name, attrs, size, mtime });
    }
    batch
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

    /// MIN-02: unreadable folders are counted instead of silently missing.
    #[test]
    fn unreadable_folders_are_counted() {
        let (work_tx, _work_rx) = unbounded();
        let skipped = AtomicUsize::new(0);
        let missing = std::env::temp_dir().join(format!("filecraft-walk-missing-{}", std::process::id()));
        let batch = read_one_dir(&missing, 0, &AtomicU32::new(1), &AtomicUsize::new(1), &skipped, &work_tx);
        assert!(batch.is_empty());
        assert_eq!(skipped.load(Ordering::Relaxed), 1);
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
