//! In-memory file index: one [`VolumeIndex`] per drive, stored as compact arrays so that
//! millions of entries can be scanned per keystroke.

use std::cmp::Ordering;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parent value of a volume's root entry.
pub const NONE: u32 = u32::MAX;

pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_REPARSE_POINT: u32 = 0x400;

/// Path depth that [`VolumeIndex::write_path`] handles without allocating.
const CHAIN_STACK: usize = 512;

/// `idx` and its ancestors up to, but not including, the root. The count bound only matters if
/// the links were corrupt; it stops a loop rather than truncating a valid path.
fn ancestors(entries: &[Entry], idx: u32) -> impl Iterator<Item = u32> + '_ {
    std::iter::successors(Some(idx), |&i| Some(entries[i as usize].parent))
        .take_while(|&i| i != 0 && i != NONE)
        .take(entries.len())
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Entry {
    name_off: u32,
    lower_off: u32,
    pub parent: u32,
    name_len: u16,
    lower_len: u16,
    /// Windows file attributes (`FILE_ATTRIBUTE_*`).
    pub attrs: u32,
    /// File size, or the total size of everything below a directory.
    pub size: u64,
    /// Last write time as a Windows FILETIME (100 ns ticks since 1601), 0 when unknown.
    pub mtime: i64,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Read directly from the NTFS master file table.
    Mft,
    /// Built by walking the directory tree.
    Walk,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Mft => "NTFS MFT",
            Source::Walk => "folder scan",
        }
    }
}

pub struct VolumeIndex {
    pub letter: char,
    pub source: Source,
    pub built_at: SystemTime,
    pub build_time: Duration,
    names: String,
    lower: String,
    entries: Vec<Entry>,
    /// Entry ids (root excluded) in ascending order of each sort key.
    pub by_name: Vec<u32>,
    pub by_path: Vec<u32>,
    pub by_size: Vec<u32>,
    pub by_date: Vec<u32>,
}

impl VolumeIndex {
    /// Number of searchable entries (the root is not counted).
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Number of entry slots (ids run from 0 to this, exclusive), the root included.
    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn entry(&self, idx: u32) -> &Entry {
        &self.entries[idx as usize]
    }

    #[inline]
    pub fn name(&self, idx: u32) -> &str {
        let e = &self.entries[idx as usize];
        arena_str(&self.names, e.name_off, e.name_len).unwrap_or("")
    }

    #[inline]
    pub fn lower_name(&self, idx: u32) -> &str {
        let e = &self.entries[idx as usize];
        arena_str(&self.lower, e.lower_off, e.lower_len).unwrap_or("")
    }

    /// Appends the full path of `idx` (e.g. `C:\Users\me\file.txt`) to `buf`, however deep.
    /// Parent links are acyclic and end at the root (checked when a cache is loaded).
    pub fn write_path(&self, idx: u32, buf: &mut String, lowercase: bool) {
        let mut stack = [0u32; CHAIN_STACK];
        let mut depth = 0;
        let mut cur = idx;
        while cur != 0 && cur != NONE && depth < CHAIN_STACK {
            stack[depth] = cur;
            depth += 1;
            cur = self.entries[cur as usize].parent;
        }
        // Deeper paths are rare: continue the chain on the heap instead of cutting it short.
        let mut heap = Vec::new();
        if cur != 0 && cur != NONE {
            heap.extend_from_slice(&stack);
            heap.extend(ancestors(&self.entries, cur));
        }
        let chain = if heap.is_empty() { &stack[..depth] } else { &heap[..] };
        buf.push(if lowercase { self.letter.to_ascii_lowercase() } else { self.letter });
        buf.push(':');
        if chain.is_empty() {
            buf.push('\\');
        }
        for &id in chain.iter().rev() {
            buf.push('\\');
            buf.push_str(if lowercase { self.lower_name(id) } else { self.name(id) });
        }
    }

    pub fn full_path(&self, idx: u32) -> String {
        let mut s = String::with_capacity(96);
        self.write_path(idx, &mut s, false);
        s
    }

    /// Path of the folder containing `idx`.
    pub fn parent_path(&self, idx: u32) -> String {
        let parent = self.entries[idx as usize].parent;
        if parent == NONE {
            return String::new();
        }
        self.full_path(parent)
    }

    pub fn order(&self, key: SortKey) -> &[u32] {
        match key {
            SortKey::Name => &self.by_name,
            SortKey::Path => &self.by_path,
            SortKey::Size => &self.by_size,
            SortKey::Modified => &self.by_date,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

/// A consistent snapshot of every indexed volume, sorted by drive letter.
#[derive(Clone, Default)]
pub struct Index {
    pub volumes: Vec<Arc<VolumeIndex>>,
}

impl Index {
    pub fn total_entries(&self) -> usize {
        self.volumes.iter().map(|v| v.len()).sum()
    }

    pub fn volume(&self, vol: u8) -> &VolumeIndex {
        &self.volumes[vol as usize]
    }

    /// Returns a new snapshot with `volume` added or replacing the one for the same drive.
    pub fn with_volume(&self, volume: Arc<VolumeIndex>) -> Index {
        let mut volumes: Vec<_> = self
            .volumes
            .iter()
            .filter(|v| v.letter != volume.letter)
            .cloned()
            .collect();
        volumes.push(volume);
        volumes.sort_by_key(|v| v.letter);
        Index { volumes }
    }

    pub fn without_volume(&self, letter: char) -> Index {
        Index {
            volumes: self.volumes.iter().filter(|v| v.letter != letter).cloned().collect(),
        }
    }
}

// ─── Building ───────────────────────────────────────────────────────────────

pub struct VolumeBuilder {
    letter: char,
    names: String,
    lower: String,
    entries: Vec<Entry>,
    /// Largest size of each name arena: entries address names with `u32` offsets.
    arena_limit: usize,
    /// Set once a name did not fit; see [`VolumeBuilder::check_capacity`].
    full: bool,
}

impl VolumeBuilder {
    /// Creates a builder whose entry 0 is the volume root.
    pub fn new(letter: char) -> Self {
        let mut b = Self {
            letter,
            names: String::with_capacity(1 << 20),
            lower: String::with_capacity(1 << 20),
            entries: Vec::with_capacity(1 << 16),
            arena_limit: u32::MAX as usize,
            full: false,
        };
        let root = format!("{letter}:");
        b.push(&root, NONE, ATTR_DIRECTORY, 0, 0);
        b
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn reserve(&mut self, entries: usize) {
        self.entries.reserve(entries);
        self.names.reserve(entries * 16);
        self.lower.reserve(entries * 16);
    }

    /// Fails when some name did not fit the name arenas (more than 4 GiB of names), instead of
    /// letting their `u32` offsets wrap onto other names (MIN-04).
    pub fn check_capacity(&self) -> io::Result<()> {
        if self.full {
            return Err(io::Error::other("the drive has more than 4 GiB of file names"));
        }
        Ok(())
    }

    fn make_entry(&mut self, name: &str, parent: u32, attrs: u32, size: u64, mtime: i64) -> Entry {
        let name = truncate_name(name);
        // ASCII names (nearly all) are lowercased in place; others may change length.
        let lowered = (!name.is_ascii()).then(|| name.to_lowercase());
        let lowered_len = lowered.as_ref().map_or(name.len(), String::len);
        if self.full
            || self.names.len() + name.len() > self.arena_limit
            || self.lower.len() + lowered_len > self.arena_limit
        {
            self.full = true;
            return Entry { parent, attrs, size, mtime, ..Entry::default() };
        }
        let name_off = self.names.len() as u32;
        self.names.push_str(name);
        let lower_off = self.lower.len() as u32;
        match &lowered {
            Some(l) => self.lower.push_str(l),
            None => self.lower.extend(name.bytes().map(|b| b.to_ascii_lowercase() as char)),
        }
        // Lowercasing can lengthen a name; cut on a character boundary so the range stays valid.
        let mut lower_end = self.lower.len().min(lower_off as usize + u16::MAX as usize);
        while !self.lower.is_char_boundary(lower_end) {
            lower_end -= 1;
        }
        let lower_len = (lower_end - lower_off as usize) as u16;
        Entry {
            name_off,
            lower_off,
            parent,
            name_len: name.len() as u16,
            lower_len,
            attrs,
            size,
            mtime,
        }
    }

    pub fn push(&mut self, name: &str, parent: u32, attrs: u32, size: u64, mtime: i64) -> u32 {
        let e = self.make_entry(name, parent, attrs, size, mtime);
        self.entries.push(e);
        (self.entries.len() - 1) as u32
    }

    /// Stores an entry at a pre-assigned id, growing the table as needed. A name that is not one
    /// path component (see [`valid_name`]) is stored as `?`, as the NTFS reader does, so the
    /// saved cache stays loadable.
    pub fn set(&mut self, idx: u32, name: &str, parent: u32, attrs: u32, size: u64, mtime: i64) {
        let name = if valid_name(name) { name } else { "?" };
        let e = self.make_entry(name, parent, attrs, size, mtime);
        let idx = idx as usize;
        if idx >= self.entries.len() {
            self.entries.resize(idx + 1, Entry { parent: NONE, ..Entry::default() });
        }
        self.entries[idx] = e;
    }

    pub fn finish(mut self, source: Source, build_time: Duration) -> VolumeIndex {
        compute_folder_sizes(&mut self.entries);

        let entries = &self.entries;
        let lower = &self.lower;
        let lower_of = |i: u32| -> &str {
            let e = &entries[i as usize];
            lower
                .get(e.lower_off as usize..e.lower_off as usize + e.lower_len as usize)
                .unwrap_or("")
        };

        // Entry 0 is the root; ids left unfilled by `set` have no parent.
        let ids: Vec<u32> = (1..entries.len() as u32)
            .filter(|&i| entries[i as usize].parent != NONE)
            .collect();

        // One sort at a time: each already uses every core, and running them side by side
        // multiplied the threads and the merge buffers by four.
        let mut by_name = ids.clone();
        par_sort_by(&mut by_name, |&a, &b| lower_of(a).cmp(lower_of(b)).then(a.cmp(&b)));
        let mut by_path = ids.clone();
        {
            let rank = directory_ranks(entries, &lower_of);
            par_sort_by(&mut by_path, |&a, &b| {
                let pa = rank[entries[a as usize].parent as usize];
                let pb = rank[entries[b as usize].parent as usize];
                pa.cmp(&pb).then_with(|| lower_of(a).cmp(lower_of(b))).then(a.cmp(&b))
            });
        }
        let mut by_size = ids.clone();
        par_sort_by(&mut by_size, |&a, &b| entries[a as usize].size.cmp(&entries[b as usize].size).then(a.cmp(&b)));
        let mut by_date = ids;
        par_sort_by(&mut by_date, |&a, &b| entries[a as usize].mtime.cmp(&entries[b as usize].mtime).then(a.cmp(&b)));

        VolumeIndex {
            letter: self.letter,
            source,
            built_at: SystemTime::now(),
            build_time,
            names: self.names,
            lower: self.lower,
            entries: self.entries,
            by_name,
            by_path,
            by_size,
            by_date,
        }
    }
}

/// Whether `name` can be one component of a path: not empty, `.` or `..`, and without a separator,
/// `:` or NUL. The folder scan never lists anything else, and the NTFS reader substitutes such
/// names. A cache holding one would make a displayed name stand for a different path (FC-17):
/// `x:y` in a folder names the stream `y` of the sibling file `x`, even through a `\\?\` path.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', ':', '\0'])
}

/// The `len` bytes at `off` in a name arena, if they lie inside it on character boundaries.
#[inline]
fn arena_str(arena: &str, off: u32, len: u16) -> Option<&str> {
    let start = off as usize;
    arena.get(start..start.checked_add(len as usize)?)
}

fn truncate_name(name: &str) -> &str {
    if name.len() <= u16::MAX as usize {
        return name;
    }
    let mut end = u16::MAX as usize;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Replaces directory sizes with the total size of the files below them.
fn compute_folder_sizes(entries: &mut [Entry]) {
    let n = entries.len();
    // Each folder starts with the sizes of the files directly inside it.
    let mut total = vec![0u64; n];
    for e in entries.iter().filter(|e| !e.is_dir() && e.parent != NONE) {
        let p = e.parent as usize;
        total[p] = total[p].saturating_add(e.size);
    }

    // Link every folder to its children, then list the folders in pre-order from the roots.
    // A folder's descendants always come after it, so walking the list backwards adds each
    // folder's finished total into its parent once: linear time, however deep the tree.
    let mut head = vec![NONE; n];
    let mut next = vec![NONE; n];
    for (d, e) in entries.iter().enumerate() {
        if e.is_dir() && e.parent != NONE {
            next[d] = head[e.parent as usize];
            head[e.parent as usize] = d as u32;
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut stack: Vec<u32> = (0..n as u32)
        .filter(|&i| entries[i as usize].is_dir() && entries[i as usize].parent == NONE)
        .collect();
    while let Some(d) = stack.pop() {
        order.push(d);
        let mut c = head[d as usize];
        while c != NONE {
            stack.push(c);
            c = next[c as usize];
        }
    }
    for &d in order.iter().rev() {
        let p = entries[d as usize].parent;
        if p != NONE {
            total[p as usize] = total[p as usize].saturating_add(total[d as usize]);
        }
    }
    for (e, t) in entries.iter_mut().zip(total) {
        if e.is_dir() {
            e.size = t;
        }
    }
}

/// Rank of every directory in full-path order (only directory slots are meaningful).
///
/// Folders are ordered by their chain of names from the root, compared name by name (a folder
/// before its subfolders, siblings by name). This is computed by a pre-order walk, so no paths are
/// built and the cost is O(n log n) however deep the tree (FC-12). It differs from sorting the
/// full path text only where one sibling's name is a prefix of another's (`a` and `a b`), where
/// the text order would interleave a folder's subfolders with its sibling's contents.
/// Folders not reachable from the root (only possible in corrupt data) are numbered last.
fn directory_ranks<'a>(entries: &[Entry], lower_of: &(impl Fn(u32) -> &'a str + Sync)) -> Vec<u32> {
    let n = entries.len();
    let mut kids: Vec<u32> = (0..n as u32)
        .filter(|&i| entries[i as usize].is_dir() && entries[i as usize].parent != NONE)
        .collect();
    kids.sort_unstable_by(|&a, &b| {
        entries[a as usize]
            .parent
            .cmp(&entries[b as usize].parent)
            .then_with(|| lower_of(a).cmp(lower_of(b)))
            .then(a.cmp(&b))
    });
    // The children of folder `p` are `kids[first[p]..first[p] + count[p]]`.
    let mut first = vec![0u32; n];
    let mut count = vec![0u32; n];
    for (pos, &d) in kids.iter().enumerate() {
        let p = entries[d as usize].parent as usize;
        if count[p] == 0 {
            first[p] = pos as u32;
        }
        count[p] += 1;
    }

    let mut rank = vec![0u32; n];
    let mut seen = vec![false; n];
    let mut next_rank = 0u32;
    let mut stack = vec![0u32];
    while let Some(d) = stack.pop() {
        let di = d as usize;
        if seen[di] {
            continue;
        }
        seen[di] = true;
        rank[di] = next_rank;
        next_rank += 1;
        let start = first[di] as usize;
        for &k in kids[start..start + count[di] as usize].iter().rev() {
            stack.push(k);
        }
    }
    for i in 0..n {
        if !seen[i] && entries[i].is_dir() {
            rank[i] = next_rank;
            next_rank += 1;
        }
    }
    rank
}

pub fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 32)
}

/// Parallel sort: sorts chunks on separate threads, then merges runs pairwise in parallel.
pub fn par_sort_by<F>(v: &mut Vec<u32>, cmp: F)
where
    F: Fn(&u32, &u32) -> Ordering + Sync,
{
    let threads = thread_count();
    if v.len() < 100_000 || threads == 1 {
        v.sort_unstable_by(&cmp);
        return;
    }
    let chunk = v.len().div_ceil(threads);
    std::thread::scope(|s| {
        for part in v.chunks_mut(chunk) {
            let cmp = &cmp;
            s.spawn(move || part.sort_unstable_by(cmp));
        }
    });

    let len = v.len();
    let mut runs: Vec<(usize, usize)> = (0..len).step_by(chunk).map(|s| (s, (s + chunk).min(len))).collect();
    let mut src = std::mem::take(v);
    let mut dst = vec![0u32; len];
    while runs.len() > 1 {
        let mut next = Vec::with_capacity(runs.len().div_ceil(2));
        std::thread::scope(|s| {
            let mut rest: &mut [u32] = &mut dst;
            for pair in runs.chunks(2) {
                let (start, end) = (pair[0].0, pair[pair.len() - 1].1);
                let (out, tail) = rest.split_at_mut(end - start);
                rest = tail;
                next.push((start, end));
                let src = &src;
                let cmp = &cmp;
                if let [a, b] = pair {
                    let (a, b) = (*a, *b);
                    s.spawn(move || merge_into(&src[a.0..a.1], &src[b.0..b.1], out, cmp));
                } else {
                    out.copy_from_slice(&src[start..end]);
                }
            }
        });
        std::mem::swap(&mut src, &mut dst);
        runs = next;
    }
    *v = src;
}

fn merge_into<F>(a: &[u32], b: &[u32], out: &mut [u32], cmp: &F)
where
    F: Fn(&u32, &u32) -> Ordering,
{
    let (mut i, mut j) = (0, 0);
    for slot in out.iter_mut() {
        if j >= b.len() || (i < a.len() && cmp(&a[i], &b[j]) != Ordering::Greater) {
            *slot = a[i];
            i += 1;
        } else {
            *slot = b[j];
            j += 1;
        }
    }
}

// ─── Disk cache ─────────────────────────────────────────────────────────────

const CACHE_MAGIC: &[u8; 8] = b"FFSIDX\0\x02";
/// Largest cache file that is read: about 40 million entries at roughly 100 bytes each.
const MAX_CACHE_BYTES: u64 = 4 << 30;
/// Most entries a cache may hold, the NTFS reader's record limit. The file size alone allowed
/// over 100 million, whose parsed tables and checks need well over 12 GiB.
const MAX_CACHE_ENTRIES: usize = 64 << 20;
/// Serialized size of one [`Entry`].
const ENTRY_BYTES: usize = 36;

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("FileCraft")
}

/// Creates `dir` if needed and checks that neither it nor any of its ancestors inside
/// [`data_dir`] is a junction, symbolic link or other reparse point. The data folder is writable
/// by unelevated code of the same user, which could otherwise aim an elevated instance's cache
/// writes, staging cleanup and backups at a protected folder (FC-18).
pub fn ensure_own_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let base = data_dir();
    let mut cur = Some(dir);
    let mut first = true;
    while let Some(d) = cur {
        if !first && !d.starts_with(&base) {
            break;
        }
        first = false;
        if is_reparse_point(&fs::symlink_metadata(d)?) {
            return Err(io::Error::other(format!(
                "{} is a link or junction; FileCraft does not write through it",
                d.display()
            )));
        }
        cur = d.parent();
    }
    Ok(())
}

fn is_reparse_point(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & ATTR_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

pub fn cache_path(letter: char) -> PathBuf {
    data_dir().join("index").join(format!("{letter}.idx"))
}

/// Serializes the check-then-rename step of saves in this process.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Staging files older than this are left over from a crash and are deleted.
const STALE_STAGING: Duration = Duration::from_secs(3600);

impl VolumeIndex {
    /// [`VolumeIndex::save_unless`] that is never superseded (tests and the benchmark).
    #[cfg(test)]
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.save_unless(path, &|| false).map(|_| ())
    }

    /// Writes the cache to `path` and returns whether it replaced it. The data goes to a staging
    /// file of its own (created exclusively, so concurrent saves never share or follow one) and
    /// is synced before it is renamed over `path`, so readers always see one whole snapshot.
    /// Nothing is replaced when `superseded` says a newer job owns this drive, or when `path`
    /// already holds a snapshot built later (by another instance): the newest snapshot wins.
    pub fn save_unless(&self, path: &Path, superseded: &dyn Fn() -> bool) -> io::Result<bool> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        ensure_own_dir(dir)?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        remove_stale_staging(dir, &stem);
        let (tmp, file) = create_staging(dir, &stem)?;
        let result = self.write_to(file).and_then(|()| {
            let _one_at_a_time = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            if superseded() || cached_build_secs(path).is_some_and(|secs| secs > self.built_secs()) {
                return Ok(false);
            }
            fs::rename(&tmp, path).map(|()| true)
        });
        if !matches!(result, Ok(true)) {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn built_secs(&self) -> u64 {
        self.built_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
    }

    fn write_to(&self, file: fs::File) -> io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, file);
        self.serialize(&mut w)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()
    }

    /// Writes the index in the cache format (also sent by the NTFS helper, see `mft_helper`).
    pub fn serialize(&self, w: &mut impl Write) -> io::Result<()> {
        {
            w.write_all(CACHE_MAGIC)?;
            w.write_all(&(self.letter as u32).to_le_bytes())?;
            w.write_all(&[matches!(self.source, Source::Mft) as u8])?;
            w.write_all(&self.built_secs().to_le_bytes())?;
            w.write_all(&(self.build_time.as_millis() as u64).to_le_bytes())?;
            for arena in [&self.names, &self.lower] {
                w.write_all(&(arena.len() as u64).to_le_bytes())?;
                w.write_all(arena.as_bytes())?;
            }
            w.write_all(&(self.entries.len() as u64).to_le_bytes())?;
            for e in &self.entries {
                w.write_all(&e.name_off.to_le_bytes())?;
                w.write_all(&e.lower_off.to_le_bytes())?;
                w.write_all(&e.parent.to_le_bytes())?;
                w.write_all(&e.name_len.to_le_bytes())?;
                w.write_all(&e.lower_len.to_le_bytes())?;
                w.write_all(&e.attrs.to_le_bytes())?;
                w.write_all(&e.size.to_le_bytes())?;
                w.write_all(&e.mtime.to_le_bytes())?;
            }
            for order in [&self.by_name, &self.by_path, &self.by_size, &self.by_date] {
                w.write_all(&(order.len() as u64).to_le_bytes())?;
                for id in order {
                    w.write_all(&id.to_le_bytes())?;
                }
            }
            Ok(())
        }
    }

    /// Reads the cache of drive `letter`. Anything malformed is an `InvalidData` error rather
    /// than a panic, so a corrupt or foreign cache is ignored and rebuilt.
    pub fn load(path: &Path, letter: char) -> io::Result<VolumeIndex> {
        let data = read_with_budget(path, MAX_CACHE_BYTES)?;
        let index = Self::parse(&data, letter)?;
        // Free the file's bytes before the tree and order checks allocate their own tables.
        drop(data);
        index.check()?;
        Ok(index)
    }

    /// Parses an index in the cache format with every check [`VolumeIndex::load`] applies.
    pub fn from_bytes(data: &[u8], letter: char) -> io::Result<VolumeIndex> {
        let index = Self::parse(data, letter)?;
        index.check()?;
        Ok(index)
    }

    /// Parses the cache format, checking every range, id and length; [`VolumeIndex::check`]
    /// must still verify the tree and the sort orders.
    fn parse(data: &[u8], letter: char) -> io::Result<VolumeIndex> {
        if data.len() as u64 > MAX_CACHE_BYTES {
            return Err(invalid("index too large"));
        }
        let mut r = Reader { data, pos: 0 };
        if r.bytes(8)? != CACHE_MAGIC {
            return Err(invalid("unknown cache format"));
        }
        if char::from_u32(r.u32()?) != Some(letter) {
            return Err(invalid("cache belongs to another drive"));
        }
        let source = match r.u8()? {
            0 => Source::Walk,
            1 => Source::Mft,
            _ => return Err(invalid("bad index source")),
        };
        let built_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(r.u64()?))
            .ok_or_else(|| invalid("bad build timestamp"))?;
        let build_time = Duration::from_millis(r.u64()?);
        let names = r.string()?;
        let lower = r.string()?;

        let count = r.len()?;
        if count > MAX_CACHE_ENTRIES {
            return Err(invalid("too many entries in cache"));
        }
        if count == 0 || count.checked_mul(ENTRY_BYTES).is_none_or(|n| n > data.len()) {
            return Err(invalid("bad entry count"));
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let e = Entry {
                name_off: r.u32()?,
                lower_off: r.u32()?,
                parent: r.u32()?,
                name_len: r.u16()?,
                lower_len: r.u16()?,
                attrs: r.u32()?,
                size: r.u64()?,
                mtime: r.u64()? as i64,
            };
            if e.parent != NONE && e.parent as usize >= count {
                return Err(invalid("bad parent id"));
            }
            let (Some(name), Some(lower_name)) = (arena_str(&names, e.name_off, e.name_len), arena_str(&lower, e.lower_off, e.lower_len)) else {
                return Err(invalid("bad name range"));
            };
            // Every linked entry (all but the root and unfilled slots) is one path component, in
            // both arenas: the lowercased name is what path terms and the selection match on.
            if i != 0 && e.parent != NONE && !(valid_name(name) && valid_name(lower_name)) {
                return Err(invalid("bad name"));
            }
            entries.push(e);
        }
        if entries[0].parent != NONE || !entries[0].is_dir() {
            return Err(invalid("bad root entry"));
        }

        // Each order lists every entry that has a parent (so not the root) exactly once.
        let listed = entries.iter().skip(1).filter(|e| e.parent != NONE).count();
        let mut seen = vec![0u8; count];
        let mut orders = Vec::with_capacity(4);
        for pass in 1..=4u8 {
            if r.len()? != listed {
                return Err(invalid("bad order length"));
            }
            let raw = r.bytes(listed * 4)?;
            let order: Vec<u32> = raw.as_chunks::<4>().0.iter().map(|&c| u32::from_le_bytes(c)).collect();
            for &id in &order {
                let i = id as usize;
                if i == 0 || i >= count || entries[i].parent == NONE || seen[i] == pass {
                    return Err(invalid("bad order id"));
                }
                seen[i] = pass;
            }
            orders.push(order);
        }
        if r.pos != data.len() {
            return Err(invalid("trailing data in cache"));
        }
        let by_date = orders.pop().unwrap();
        let by_size = orders.pop().unwrap();
        let by_path = orders.pop().unwrap();
        let by_name = orders.pop().unwrap();
        Ok(VolumeIndex {
            letter,
            source,
            built_at,
            build_time,
            names,
            lower,
            entries,
            by_name,
            by_path,
            by_size,
            by_date,
        })
    }

    /// The checks of a parsed cache that need tables of their own: every entry reaches the root,
    /// and the orders are sorted the way [`VolumeBuilder::finish`] sorts them.
    fn check(&self) -> io::Result<()> {
        check_tree(&self.entries)?;
        check_orders(&self.entries, &self.lower, &self.by_name, &self.by_path, &self.by_size, &self.by_date)
    }
}

/// Checks that a loaded cache's orders are sorted exactly the way [`VolumeBuilder::finish`]
/// sorts them (MIN-04): name, size and date in linear time; path after recomputing the folders'
/// ranks, which sorts the folders only.
fn check_orders(entries: &[Entry], lower: &str, by_name: &[u32], by_path: &[u32], by_size: &[u32], by_date: &[u32]) -> io::Result<()> {
    let lower_of = |i: u32| {
        let e = &entries[i as usize];
        arena_str(lower, e.lower_off, e.lower_len).unwrap_or("")
    };
    let by_lower = |a: u32, b: u32| lower_of(a).cmp(lower_of(b)).then(a.cmp(&b));
    let increasing = |order: &[u32], cmp: &dyn Fn(u32, u32) -> Ordering| order.windows(2).all(|w| cmp(w[0], w[1]) == Ordering::Less);

    let sorted = increasing(by_name, &by_lower)
        && increasing(by_size, &|a, b| entries[a as usize].size.cmp(&entries[b as usize].size).then(a.cmp(&b)))
        && increasing(by_date, &|a, b| entries[a as usize].mtime.cmp(&entries[b as usize].mtime).then(a.cmp(&b)));
    if !sorted {
        return Err(invalid("sort order not sorted"));
    }

    // The same key `finish` sorts by. Ranking the folders sorts only the folders, a fraction of
    // the entries, so this stays cheap next to reading the cache.
    let rank = directory_ranks(entries, &lower_of);
    let by_parent_then_name = |a: u32, b: u32| {
        let (pa, pb) = (entries[a as usize].parent, entries[b as usize].parent);
        rank[pa as usize].cmp(&rank[pb as usize]).then_with(|| by_lower(a, b))
    };
    if !increasing(by_path, &by_parent_then_name) {
        return Err(invalid("path order not sorted"));
    }
    Ok(())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Creates `<stem>.<pid>-<n>.tmp` in `dir`, failing rather than reusing (or following) a name
/// that already exists.
fn create_staging(dir: &Path, stem: &str) -> io::Result<(PathBuf, fs::File)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!("{stem}.{}-{n}.tmp", std::process::id()));
        match fs::File::options().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Deletes staging files of `stem` that a crashed save left behind (including the old fixed
/// `<stem>.tmp` name). Recent ones may belong to a save in progress and are kept.
fn remove_stale_staging(dir: &Path, stem: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_STAGING));
        if name.starts_with(&format!("{stem}.")) && name.ends_with(".tmp") && stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Build time (seconds since 1970) recorded in the cache at `path`, if it is a FileCraft cache.
/// A stamp further ahead of the clock than [`CLOCK_SKEW_SECS`] is not a newer snapshot (clock
/// skew or corruption), so it is reported as absent and cannot block the save (FC-11).
fn cached_build_secs(path: &Path) -> Option<u64> {
    let mut header = [0u8; 21];
    fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    let secs = (&header[..8] == CACHE_MAGIC).then(|| u64::from_le_bytes(header[13..21].try_into().unwrap()))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    (secs <= now.saturating_add(CLOCK_SKEW_SECS)).then_some(secs)
}

/// How far ahead of the local clock a cache build time may be before it is treated as bogus.
const CLOCK_SKEW_SECS: u64 = 300;

/// Reads a whole file, refusing one larger than `budget` bytes.
fn read_with_budget(path: &Path, budget: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > budget {
        return Err(invalid("cache file too large"));
    }
    let mut data = Vec::with_capacity(usize::try_from(len).map_err(|_| invalid("cache file too large"))?);
    // `take` keeps the budget even if the file grows while it is read.
    file.take(budget + 1).read_to_end(&mut data)?;
    if data.len() as u64 > budget {
        return Err(invalid("cache file too large"));
    }
    Ok(data)
}

/// Requires every entry with a parent to reach the root (entry 0) through its parent links:
/// no loops, and no ancestor that is an unused slot (no parent, but not the root). Paths can
/// then always be rebuilt in full rather than silently attached to the drive root.
fn check_tree(entries: &[Entry]) -> io::Result<()> {
    const UNCHECKED: u8 = 0;
    const ON_CHAIN: u8 = 1;
    const ROOTED: u8 = 2;
    const UNUSED: u8 = 3;
    let mut state = vec![UNCHECKED; entries.len()];
    let mut chain = Vec::new();
    for start in 0..entries.len() as u32 {
        let mut cur = start;
        while state[cur as usize] == UNCHECKED {
            let parent = entries[cur as usize].parent;
            if parent == NONE {
                state[cur as usize] = if cur == 0 { ROOTED } else { UNUSED };
                break;
            }
            state[cur as usize] = ON_CHAIN;
            chain.push(cur);
            cur = parent;
        }
        match state[cur as usize] {
            ON_CHAIN => return Err(invalid("parent links form a loop")),
            UNUSED if !chain.is_empty() => return Err(invalid("entry is not attached to the root")),
            _ => {}
        }
        for id in chain.drain(..) {
            state[id as usize] = ROOTED;
        }
    }
    Ok(())
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or_else(|| invalid("truncated cache"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    /// A `u64` length or count that must also fit in `usize`.
    fn len(&mut self) -> io::Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| invalid("length out of range"))
    }
    fn string(&mut self) -> io::Result<String> {
        let n = self.len()?;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| invalid("bad name table"))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// C:\ with: docs\ (readme.md 100, Notes.TXT 50), src\main.rs 300, big.bin 1000
    pub fn sample() -> VolumeIndex {
        let mut b = VolumeBuilder::new('C');
        let docs = b.push("docs", 0, ATTR_DIRECTORY, 0, 30);
        b.push("readme.md", docs, 0, 100, 10);
        b.push("Notes.TXT", docs, ATTR_HIDDEN, 50, 20);
        let src = b.push("src", 0, ATTR_DIRECTORY, 0, 40);
        b.push("main.rs", src, 0, 300, 50);
        b.push("big.bin", 0, 0, 1000, 5);
        b.finish(Source::Walk, Duration::ZERO)
    }

    fn names(v: &VolumeIndex, order: &[u32]) -> Vec<String> {
        order.iter().map(|&i| v.name(i).to_string()).collect()
    }

    #[test]
    fn builds_paths_sizes_and_orders() {
        let v = sample();
        assert_eq!(v.len(), 6);
        let main = v.by_name.iter().copied().find(|&i| v.name(i) == "main.rs").unwrap();
        assert_eq!(v.full_path(main), r"C:\src\main.rs");
        assert_eq!(v.parent_path(main), r"C:\src");
        assert_eq!(v.full_path(0), r"C:\");

        let docs = v.by_name.iter().copied().find(|&i| v.name(i) == "docs").unwrap();
        assert_eq!(v.entry(docs).size, 150);

        assert_eq!(names(&v, &v.by_name), ["big.bin", "docs", "main.rs", "Notes.TXT", "readme.md", "src"]);
        assert_eq!(names(&v, &v.by_path), ["big.bin", "docs", "src", "Notes.TXT", "readme.md", "main.rs"]);
        assert_eq!(names(&v, &v.by_size)[5], "big.bin");
        assert_eq!(names(&v, &v.by_date)[0], "big.bin");
    }

    /// FC-19: folder sizes that add up past `u64::MAX` panicked in debug builds and wrapped in
    /// release builds. They must saturate.
    #[test]
    fn folder_sizes_saturate_instead_of_overflowing() {
        let mut b = VolumeBuilder::new('C');
        let dir = b.push("huge", 0, ATTR_DIRECTORY, 0, 0);
        b.push("a.bin", dir, 0, 1 << 63, 0);
        b.push("b.bin", dir, 0, 1 << 63, 0);
        let v = b.finish(Source::Walk, Duration::ZERO);
        let huge = v.by_name.iter().copied().find(|&i| v.name(i) == "huge").unwrap();
        assert_eq!(v.entry(huge).size, u64::MAX);
    }

    /// MIN-04: names address the arenas with `u32` offsets, which used to wrap silently past
    /// 4 GiB. A builder that runs out of arena space now reports it instead.
    #[test]
    fn name_arena_overflow_is_reported() {
        let mut b = VolumeBuilder::new('C');
        b.arena_limit = 11; // "C:" (2 bytes) is already stored
        b.push("abcde", 0, 0, 1, 0);
        assert!(b.check_capacity().is_ok());
        b.push("fghij", 0, 0, 1, 0);
        assert!(b.check_capacity().is_err());
        assert!(b.names.len() <= 11 && b.lower.len() <= 11);
    }

    /// FC-12: folder sizes and path ranks used to cost O(depth) per entry (quadratic on a deep
    /// chain). A 20,000-level chain must now build quickly and still get its sizes right.
    #[test]
    fn deep_chain_builds_in_linear_time() {
        const DEPTH: u32 = 20_000;
        let mut b = VolumeBuilder::new('C');
        let mut parent = 0;
        for _ in 0..DEPTH {
            parent = b.push("d", parent, ATTR_DIRECTORY, 0, 0);
        }
        let deepest = parent;
        b.push("f.bin", deepest, 0, 7, 0);
        let start = std::time::Instant::now();
        let v = b.finish(Source::Walk, Duration::ZERO);
        let took = start.elapsed();
        assert!(took < Duration::from_secs(20), "finish() took {took:?} for {DEPTH} levels");
        assert_eq!(v.entry(0).size, 7);
        assert_eq!(v.entry(deepest).size, 7);
        assert_eq!(v.len() as u32, DEPTH + 1);
    }

    /// FC-12: the path sort orders entries by their parent folder's chain of names (compared name
    /// by name), then by name. Checked against a direct sort of those chains, on siblings whose
    /// names are prefixes of each other (`a`, `a b`, `ab`) and on names below the separator.
    #[test]
    fn path_order_is_the_order_of_name_chains() {
        let mut b = VolumeBuilder::new('C');
        for name in ["a", "a b", "a-b", "ab", "a0", "b", "Ac"] {
            let d = b.push(name, 0, ATTR_DIRECTORY, 0, 0);
            let sub = b.push("z", d, ATTR_DIRECTORY, 0, 0);
            b.push("f1", sub, 0, 1, 0);
            b.push("f2", d, 0, 1, 0);
        }
        let v = b.finish(Source::Walk, Duration::ZERO);
        let parent_chain = |i: u32| -> Vec<String> {
            let mut chain = Vec::new();
            let mut cur = v.entry(i).parent;
            while cur != 0 && cur != NONE {
                chain.push(v.name(cur).to_lowercase());
                cur = v.entry(cur).parent;
            }
            chain.reverse();
            chain
        };
        let mut expected = v.by_path.clone();
        expected.sort_by_key(|&i| (parent_chain(i), v.name(i).to_lowercase(), i));
        assert_eq!(v.by_path, expected);
    }

    #[test]
    fn par_sort_matches_std_sort() {
        let mut v: Vec<u32> = (0..250_000u32).map(|i| i.wrapping_mul(2_654_435_761) % 100_003).collect();
        let mut expected = v.clone();
        expected.sort_unstable();
        par_sort_by(&mut v, |a, b| a.cmp(b));
        assert_eq!(v, expected);
    }

    #[test]
    fn cache_round_trip() {
        let v = sample();
        let path = std::env::temp_dir().join(format!("filecraft-test-{}.idx", std::process::id()));
        v.save(&path).unwrap();
        let loaded = VolumeIndex::load(&path, 'C').unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(loaded.letter, 'C');
        assert_eq!(names(&loaded, &loaded.by_path), names(&v, &v.by_path));
        let main = loaded.by_name[2];
        assert_eq!(loaded.full_path(main), r"C:\src\main.rs");
        assert_eq!(loaded.entry(main).size, 300);
    }

    fn cache_bytes(v: &VolumeIndex) -> Vec<u8> {
        // Tests run in parallel, so every call gets its own file.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("filecraft-test-bytes-{}-{n}.idx", std::process::id()));
        v.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        bytes
    }

    /// Loads `bytes` as a cache of drive `letter` and returns the error it must produce.
    fn load_error(label: &str, bytes: &[u8], letter: char) -> io::Error {
        let path = std::env::temp_dir().join(format!("filecraft-test-{label}-{}.idx", std::process::id()));
        fs::write(&path, bytes).unwrap();
        let result = VolumeIndex::load(&path, letter);
        let _ = fs::remove_file(&path);
        let err = result.err().unwrap_or_else(|| panic!("{label}: malformed cache was accepted"));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{label}: {err}");
        err
    }

    /// Makes `link` a directory junction to `target` (no privilege needed, unlike symlinks).
    #[cfg(windows)]
    pub fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
        assert!(out.status.success(), "mklink failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// FC-18: a junction planted on the cache folder used to redirect cache writes (and the
    /// staging cleanup) of an elevated instance into whatever folder it pointed at.
    #[cfg(windows)]
    #[test]
    fn cache_saves_do_not_write_through_junctions() {
        let dir = cache_dir("junction");
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = dir.join("link");
        junction(&link, &target);

        let err = sample().save(&link.join("C.idx")).unwrap_err();
        assert!(err.to_string().contains("link or junction"), "{err}");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// FC-17: a cache entry whose name is a path (`..\..\Windows\win.ini`) or not a usable path
    /// component made a displayed name stand for a different file. Such caches are rejected.
    #[test]
    fn caches_with_path_like_names_are_rejected() {
        for (label, bad) in [
            ("traversal", r"..\..\Windows\win.ini"),
            ("dotdot", ".."),
            ("dot", "."),
            ("empty", ""),
            ("slash", "a/b"),
            ("nul", "a\0b"),
            ("stream", "notes.txt:payload"),
        ] {
            let mut b = VolumeBuilder::new('C');
            let dir = b.push("Public", 0, ATTR_DIRECTORY, 0, 0);
            b.push(bad, dir, 0, 1, 0);
            load_error(&format!("name-{label}"), &cache_bytes(&b.finish(Source::Walk, Duration::ZERO)), 'C');
        }
        // The lowercased copy is checked too: `ab` whose lowercase name was rewritten to `a\`.
        let mut b = VolumeBuilder::new('C');
        b.push("ab", 0, 0, 1, 0);
        let v = b.finish(Source::Walk, Duration::ZERO);
        let lower_at = 8 + 4 + 1 + 8 + 8 + (8 + v.names.len()) + 8;
        let mut bytes = cache_bytes(&v);
        assert_eq!(&bytes[lower_at..lower_at + 4], b"c:ab");
        bytes[lower_at + 3] = b'\\';
        load_error("lower-name", &bytes, 'C');
        // Names that are odd but valid on NTFS still load. The folder scan stores a name that is
        // not a path component as `?`, so its caches load too.
        let mut b = VolumeBuilder::new('C');
        b.push("...x", 0, 0, 1, 0);
        b.push("a\"b?", 0, 0, 1, 0);
        b.set(3, "notes.txt:payload", 0, 0, 1, 0);
        let v = b.finish(Source::Walk, Duration::ZERO);
        let path = cache_dir("odd-names").join("C.idx");
        v.save(&path).unwrap();
        let loaded = VolumeIndex::load(&path, 'C').unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.name(3), "?");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// MIN-04: the sort orders were only checked to list every entry once, not to be sorted, so
    /// a tampered cache showed results out of order. Swapping two ids in any order is rejected.
    #[test]
    fn unsorted_cache_orders_are_rejected() {
        let v = sample();
        let good = cache_bytes(&v);
        let entries_at = 8 + 4 + 1 + 8 + 8 + (8 + v.names.len()) + (8 + v.lower.len()) + 8;
        let orders_at = entries_at + v.entries.len() * ENTRY_BYTES;
        let listed = v.len();
        for (k, label) in ["name", "path", "size", "date"].iter().enumerate() {
            let first = orders_at + k * (8 + listed * 4) + 8;
            let mut bytes = good.clone();
            let (a, b) = bytes[first..first + 8].split_at_mut(4);
            a.swap_with_slice(b);
            load_error(&format!("unsorted-{label}"), &bytes, 'C');
        }

        // Folder groups that are each contiguous and sorted, but in the wrong order: `src`'s
        // entries moved ahead of `docs`'. Only a check of the folders' own order catches this.
        let path_at = orders_at + (8 + listed * 4) + 8;
        let ids: Vec<u32> = good[path_at..path_at + listed * 4].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let names_of = |ids: &[u32]| ids.iter().map(|&i| v.name(i).to_string()).collect::<Vec<_>>();
        assert_eq!(names_of(&ids[3..]), ["Notes.TXT", "readme.md", "main.rs"]);
        let mut moved = ids.clone();
        moved[3..].rotate_right(1);
        let mut bytes = good.clone();
        for (k, id) in moved.iter().enumerate() {
            bytes[path_at + 4 * k..path_at + 4 * k + 4].copy_from_slice(&id.to_le_bytes());
        }
        load_error("folders-out-of-order", &bytes, 'C');
    }

    /// A cache's entry count was bounded only by its size: a 4 GiB file could declare over 100
    /// million entries, whose tables and checks need more than 12 GiB. Counts above the NTFS
    /// reader's record limit are rejected before anything is allocated for them.
    #[test]
    fn caches_with_too_many_entries_are_rejected() {
        let v = sample();
        let count_at = 8 + 4 + 1 + 8 + 8 + (8 + v.names.len()) + (8 + v.lower.len());
        let mut bytes = cache_bytes(&v);
        assert_eq!(bytes[count_at..count_at + 8], (v.entries.len() as u64).to_le_bytes());
        bytes[count_at..count_at + 8].copy_from_slice(&(MAX_CACHE_ENTRIES as u64 + 1).to_le_bytes());
        let err = load_error("too-many-entries", &bytes, 'C');
        assert!(err.to_string().contains("too many entries"), "{err}");
    }

    /// FC-02: the audit's 21-byte cache with an overflowing timestamp used to panic (and abort).
    #[test]
    fn overflowing_timestamp_is_an_error_not_a_panic() {
        let mut bytes = CACHE_MAGIC.to_vec();
        bytes.extend_from_slice(&('C' as u32).to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        load_error("overflow", &bytes, 'C');
    }

    #[test]
    fn malformed_caches_are_rejected() {
        let v = sample();
        let good = cache_bytes(&v);
        let entries_at = 8 + 4 + 1 + 8 + 8 + (8 + v.names.len()) + (8 + v.lower.len()) + 8;
        let entry = |i: usize| entries_at + i * ENTRY_BYTES;
        let orders_at = entry(v.entries.len());
        let patch = |at: usize, value: u32| {
            let mut bytes = good.clone();
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            bytes
        };

        load_error("other-drive", &good, 'D');
        load_error("truncated", &good[..good.len() - 1], 'C');
        load_error("trailing", &[good.as_slice(), &[0]].concat(), 'C');
        load_error("bad-source", &[&good[..12], &[7], &good[13..]].concat(), 'C');
        load_error("name-range", &patch(entry(1), u32::MAX), 'C');
        load_error("parent-loop", &patch(entry(1) + 8, 1), 'C');
        load_error("root-parent", &patch(entry(0) + 8, 1), 'C');
        let second_id = u32::from_le_bytes(good[orders_at + 12..orders_at + 16].try_into().unwrap());
        load_error("duplicate-order-id", &patch(orders_at + 8, second_id), 'C');
        load_error("order-lists-root", &patch(orders_at + 8, 0), 'C');
    }

    /// FC-06: a path deeper than 512 levels used to lose its top folders and hang off the root.
    #[test]
    fn deep_paths_are_rebuilt_in_full() {
        let mut b = VolumeBuilder::new('C');
        let mut parent = 0;
        for _ in 0..513 {
            parent = b.push("d", parent, ATTR_DIRECTORY, 0, 0);
        }
        let file = b.push("victim.txt", parent, 0, 7, 0);
        let v = b.finish(Source::Walk, Duration::ZERO);

        let expected = format!(r"C:{}\victim.txt", r"\d".repeat(513));
        assert_eq!(v.full_path(file), expected);
        assert_eq!(v.parent_path(file), format!("C:{}", r"\d".repeat(513)));
        assert_eq!(v.entry(0).size, 7, "folder sizes reach the root");
        assert_eq!(v.by_path, (1..=file).collect::<Vec<_>>());

        let path = std::env::temp_dir().join(format!("filecraft-test-deep-{}.idx", std::process::id()));
        v.save(&path).unwrap();
        let loaded = VolumeIndex::load(&path, 'C');
        let _ = fs::remove_file(&path);
        assert_eq!(loaded.unwrap().full_path(file), expected);
    }

    #[test]
    fn entries_detached_from_the_root_are_rejected() {
        let mut b = VolumeBuilder::new('C');
        // Slots 1 and 2 stay unused, so the file's parent is not a real folder.
        b.set(3, "orphan.txt", 2, 0, 1, 0);
        let v = b.finish(Source::Walk, Duration::ZERO);
        load_error("detached", &cache_bytes(&v), 'C');
    }

    fn cache_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filecraft-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staging_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    /// FC-07: concurrent saves of one drive used to share `C.tmp` and fail or interleave.
    #[test]
    fn concurrent_saves_do_not_collide() {
        let dir = cache_dir("concurrent-save");
        let path = dir.join("C.idx");
        let v = sample();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|_| s.spawn(|| v.save(&path))).collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
        });
        assert_eq!(VolumeIndex::load(&path, 'C').unwrap().len(), v.len());
        assert!(staging_files(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// FC-07: a superseded job, or an older snapshot, never replaces the cache.
    #[test]
    fn superseded_or_older_snapshots_are_not_published() {
        let dir = cache_dir("superseded-save");
        let path = dir.join("C.idx");
        let newer = sample();
        assert!(!newer.save_unless(&path, &|| true).unwrap());
        assert!(!path.exists());

        assert!(newer.save_unless(&path, &|| false).unwrap());
        let mut older = sample();
        older.built_at = newer.built_at - Duration::from_secs(60);
        assert!(!older.save_unless(&path, &|| false).unwrap());
        assert_eq!(VolumeIndex::load(&path, 'C').unwrap().built_secs(), newer.built_secs());
        assert!(staging_files(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// FC-11: a cache stamped in the future (clock skew, corruption) must not block every later save.
    #[test]
    fn future_dated_cache_does_not_block_saves() {
        let dir = cache_dir("future-cache");
        let path = dir.join("C.idx");
        let mut future = sample();
        future.built_at = SystemTime::now() + Duration::from_secs(86_400);
        assert!(future.save_unless(&path, &|| false).unwrap());

        let current = sample();
        assert!(current.save_unless(&path, &|| false).unwrap());
        assert!(VolumeIndex::load(&path, 'C').unwrap().built_secs() <= current.built_secs() + 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_staging_files_are_removed() {
        let dir = cache_dir("stale-staging");
        let old = dir.join("C.tmp");
        fs::write(&old, b"left over").unwrap();
        fs::File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - 2 * STALE_STAGING).unwrap();
        let recent = dir.join("C.999-0.tmp");
        fs::write(&recent, b"in progress").unwrap();
        sample().save(&dir.join("C.idx")).unwrap();
        assert_eq!(staging_files(&dir), ["C.999-0.tmp"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
