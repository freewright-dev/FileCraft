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
}

impl VolumeBuilder {
    /// Creates a builder whose entry 0 is the volume root.
    pub fn new(letter: char) -> Self {
        let mut b = Self {
            letter,
            names: String::with_capacity(1 << 20),
            lower: String::with_capacity(1 << 20),
            entries: Vec::with_capacity(1 << 16),
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

    fn make_entry(&mut self, name: &str, parent: u32, attrs: u32, size: u64, mtime: i64) -> Entry {
        let name = truncate_name(name);
        let name_off = self.names.len() as u32;
        self.names.push_str(name);
        let lower_off = self.lower.len() as u32;
        if name.is_ascii() {
            self.lower.extend(name.bytes().map(|b| b.to_ascii_lowercase() as char));
        } else {
            self.lower.push_str(&name.to_lowercase());
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

    /// Stores an entry at a pre-assigned id, growing the table as needed.
    pub fn set(&mut self, idx: u32, name: &str, parent: u32, attrs: u32, size: u64, mtime: i64) {
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

        let (by_name, by_path, by_size, by_date) = std::thread::scope(|s| {
            let name = s.spawn(|| {
                let mut v = ids.clone();
                par_sort_by(&mut v, |&a, &b| lower_of(a).cmp(lower_of(b)).then(a.cmp(&b)));
                v
            });
            let path = s.spawn(|| {
                let rank = directory_ranks(entries, self.letter, &lower_of);
                let mut v = ids.clone();
                par_sort_by(&mut v, |&a, &b| {
                    let pa = rank[entries[a as usize].parent as usize];
                    let pb = rank[entries[b as usize].parent as usize];
                    pa.cmp(&pb).then_with(|| lower_of(a).cmp(lower_of(b))).then(a.cmp(&b))
                });
                v
            });
            let size = s.spawn(|| {
                let mut v = ids.clone();
                par_sort_by(&mut v, |&a, &b| {
                    entries[a as usize].size.cmp(&entries[b as usize].size).then(a.cmp(&b))
                });
                v
            });
            let date = s.spawn(|| {
                let mut v = ids.clone();
                par_sort_by(&mut v, |&a, &b| {
                    entries[a as usize].mtime.cmp(&entries[b as usize].mtime).then(a.cmp(&b))
                });
                v
            });
            (
                name.join().expect("name sort"),
                path.join().expect("path sort"),
                size.join().expect("size sort"),
                date.join().expect("date sort"),
            )
        });

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
    for e in entries.iter_mut().filter(|e| e.is_dir()) {
        e.size = 0;
    }
    for i in 0..entries.len() {
        let e = entries[i];
        if e.is_dir() || e.size == 0 || e.parent == NONE {
            continue;
        }
        // Every ancestor, however deep; the count bound only stops corrupt (looping) links.
        let mut p = e.parent;
        for _ in 0..entries.len() {
            if p == NONE {
                break;
            }
            let parent = &mut entries[p as usize];
            parent.size += e.size;
            p = parent.parent;
        }
    }
}

/// Rank of every directory in full-path order (only directory slots are meaningful).
fn directory_ranks<'a>(entries: &[Entry], letter: char, lower_of: &(impl Fn(u32) -> &'a str + Sync)) -> Vec<u32> {
    let mut dirs: Vec<(String, u32)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.is_dir() || (i != 0 && e.parent == NONE) {
            continue;
        }
        let chain: Vec<u32> = ancestors(entries, i as u32).collect();
        let mut path = String::with_capacity(64);
        path.push(letter.to_ascii_lowercase());
        path.push(':');
        for &id in chain.iter().rev() {
            path.push('\\');
            path.push_str(lower_of(id));
        }
        dirs.push((path, i as u32));
    }
    dirs.sort_unstable();
    let mut rank = vec![0u32; entries.len()];
    for (r, (_, idx)) in dirs.iter().enumerate() {
        rank[*idx as usize] = r as u32;
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
/// Serialized size of one [`Entry`].
const ENTRY_BYTES: usize = 36;

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("FileCraft")
}

pub fn cache_path(letter: char) -> PathBuf {
    data_dir().join("index").join(format!("{letter}.idx"))
}

impl VolumeIndex {
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
            w.write_all(CACHE_MAGIC)?;
            w.write_all(&(self.letter as u32).to_le_bytes())?;
            w.write_all(&[matches!(self.source, Source::Mft) as u8])?;
            let built = self.built_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            w.write_all(&built.to_le_bytes())?;
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
            w.flush()?;
        }
        fs::rename(&tmp, path)
    }

    /// Reads the cache of drive `letter`. Anything malformed is an `InvalidData` error rather
    /// than a panic, so a corrupt or foreign cache is ignored and rebuilt.
    pub fn load(path: &Path, letter: char) -> io::Result<VolumeIndex> {
        let data = read_with_budget(path, MAX_CACHE_BYTES)?;
        let mut r = Reader { data: &data, pos: 0 };
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
        if count == 0 || count > (u32::MAX - 1) as usize || count.checked_mul(ENTRY_BYTES).is_none_or(|n| n > data.len()) {
            return Err(invalid("bad entry count"));
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
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
            if arena_str(&names, e.name_off, e.name_len).is_none() || arena_str(&lower, e.lower_off, e.lower_len).is_none() {
                return Err(invalid("bad name range"));
            }
            entries.push(e);
        }
        if entries[0].parent != NONE || !entries[0].is_dir() {
            return Err(invalid("bad root entry"));
        }
        check_tree(&entries)?;

        // Each order lists every entry that has a parent (so not the root) exactly once.
        let listed = entries.iter().skip(1).filter(|e| e.parent != NONE).count();
        let mut seen = vec![0u8; count];
        let mut orders = Vec::with_capacity(4);
        for pass in 1..=4u8 {
            if r.len()? != listed {
                return Err(invalid("bad order length"));
            }
            let raw = r.bytes(listed * 4)?;
            let order: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
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
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

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
}
