//! Everything-style query language and the parallel search over an [`Index`].
//!
//! Syntax (terms are ANDed, case-insensitive unless "Match case" is on):
//! - `foo bar`         names containing both `foo` and `bar`
//! - `"foo bar"`       literal text with a space
//! - `!foo`            names not containing `foo`
//! - `foo|bar`         names containing `foo` or `bar`
//! - `*.rs`, `a?c`     wildcards match the whole name
//! - `src\main`        a term with `\` (or `/`) matches against the full path
//! - `ext:rs;toml`     extension filter
//! - `file:` `folder:` entry type
//! - `size:>10mb`      size filter (`>`, `>=`, `<`, `<=`, `=`, `1mb..10mb`, `empty`)
//! - `dm:today`        modified date (`today`, `yesterday`, `24h`, `7d`, `2w`, `1y`,
//!   `2026-10-13`, `>2026-01-01`, `2026-01-01..2026-02-01`)
//! - `regex:^a.*z$`    regular expression on the name
//! - `path:foo`        force a term to match the full path

use crate::index::{Index, SortKey, VolumeIndex, thread_count};
use crate::util::{TICKS_PER_DAY, TICKS_PER_SEC, local_midnight, local_midnight_today, now_filetime, parse_date};
use crossbeam_channel::{Receiver, Sender, unbounded};
use globset::{GlobBuilder, GlobMatcher};
use memchr::memmem;
use regex::{Regex, RegexBuilder};
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SearchOptions {
    pub match_case: bool,
    pub match_path: bool,
    pub regex: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Name, descending: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Hit {
    pub vol: u8,
    pub idx: u32,
}

// ─── Query parsing ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Cmp {
    Lt(i64),
    Le(i64),
    Gt(i64),
    Ge(i64),
    /// Half-open range `[lo, hi)`.
    Range(i64, i64),
}

impl Cmp {
    fn matches(self, v: i64) -> bool {
        match self {
            Cmp::Lt(x) => v < x,
            Cmp::Le(x) => v <= x,
            Cmp::Gt(x) => v > x,
            Cmp::Ge(x) => v >= x,
            Cmp::Range(lo, hi) => v >= lo && v < hi,
        }
    }
}

enum Test {
    Text { finder: memmem::Finder<'static>, path: bool },
    Wildcard { glob: GlobMatcher, path: bool },
    Regex { re: Regex, path: bool },
    Ext(Vec<String>),
    Folder,
    File,
    Size(Cmp),
    Modified(Cmp),
}

/// One search term: matches when any alternative matches (`a|b`), inverted by `!`.
struct Clause {
    alts: Vec<Test>,
    negate: bool,
}

pub struct Query {
    clauses: Vec<Clause>,
    case: bool,
}

impl Query {
    pub fn parse(text: &str, opts: SearchOptions) -> Result<Query, String> {
        let case = opts.match_case;
        let mut clauses = Vec::new();

        if opts.regex {
            let text = text.trim();
            if !text.is_empty() {
                let re = RegexBuilder::new(text)
                    .case_insensitive(!case)
                    .build()
                    .map_err(|e| short_error(&e.to_string()))?;
                clauses.push(Clause { alts: vec![Test::Regex { re, path: opts.match_path }], negate: false });
            }
            return Ok(Query { clauses, case });
        }

        for (token, quoted) in tokenize(text) {
            let (negate, body) = match token.strip_prefix('!') {
                Some(rest) if !quoted => (true, rest.to_string()),
                _ => (false, token),
            };
            if body.is_empty() {
                continue;
            }
            // `folder:src` = folders whose name contains `src` (the type test is ANDed).
            let mut body = body;
            if !quoted && let Some((prefix, value)) = body.split_once(':') {
                let type_test = match prefix.to_ascii_lowercase().as_str() {
                    "file" | "files" => Some(Test::File),
                    "folder" | "folders" | "dir" => Some(Test::Folder),
                    _ => None,
                };
                if let Some(t) = type_test {
                    clauses.push(Clause { alts: vec![t], negate: negate && value.is_empty() });
                    if value.is_empty() {
                        continue;
                    }
                    body = value.to_string();
                }
            }
            let alts = if quoted {
                vec![text_test(&body, opts.match_path, case)]
            } else {
                parse_term(&body, opts, case)?
            };
            if !alts.is_empty() {
                clauses.push(Clause { alts, negate });
            }
        }
        Ok(Query { clauses, case })
    }

    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }
}

fn short_error(msg: &str) -> String {
    msg.lines().rfind(|l| !l.trim().is_empty()).unwrap_or(msg).trim().to_string()
}

/// Splits on whitespace, keeping `"quoted parts"` together. Returns `(token, was_quoted)`.
fn tokenize(text: &str) -> Vec<(String, bool)> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    tokens.push((std::mem::take(&mut cur), quoted));
                }
                quoted = false;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push((cur, quoted));
    }
    tokens
}

fn parse_term(term: &str, opts: SearchOptions, case: bool) -> Result<Vec<Test>, String> {
    if let Some((prefix, value)) = term.split_once(':') {
        let p = prefix.to_ascii_lowercase();
        match p.as_str() {
            "ext" => {
                let exts = value
                    .split([';', ',', '|'])
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
                return Ok(vec![Test::Ext(exts)]);
            }
            "size" => return Ok(vec![Test::Size(parse_size_cmp(value)?)]),
            "dm" | "datemodified" | "modified" => return Ok(vec![Test::Modified(parse_date_cmp(value)?)]),
            "regex" => {
                let re = RegexBuilder::new(value)
                    .case_insensitive(!case)
                    .build()
                    .map_err(|e| short_error(&e.to_string()))?;
                return Ok(vec![Test::Regex { re, path: opts.match_path }]);
            }
            "path" => {
                return value
                    .split('|')
                    .filter(|a| !a.is_empty())
                    .map(|a| plain_test(a, true, case))
                    .collect();
            }
            // Drive letters like `C:` or `c:\users` are ordinary path text.
            _ => {}
        }
    }
    term.split('|')
        .filter(|a| !a.is_empty())
        .map(|a| plain_test(a, opts.match_path, case))
        .collect()
}

fn plain_test(text: &str, path: bool, case: bool) -> Result<Test, String> {
    let normalized = text.replace('/', "\\");
    let path = path || normalized.contains('\\');
    if normalized.contains(['*', '?']) {
        let glob = GlobBuilder::new(&normalized)
            .case_insensitive(!case)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .map_err(|e| short_error(&e.to_string()))?
            .compile_matcher();
        return Ok(Test::Wildcard { glob, path });
    }
    Ok(text_test(&normalized, path, case))
}

fn text_test(text: &str, path: bool, case: bool) -> Test {
    let needle = if case { text.to_string() } else { text.to_lowercase() };
    let path = path || needle.contains('\\');
    Test::Text { finder: memmem::Finder::new(needle.as_bytes()).into_owned(), path }
}

fn parse_size_cmp(value: &str) -> Result<Cmp, String> {
    let v = value.trim().to_ascii_lowercase();
    if v == "empty" {
        return Ok(Cmp::Le(0));
    }
    // Sizes outside 0..i64::MAX bytes are errors; nothing saturates or wraps.
    let after = |n: i64| n.checked_add(1).ok_or_else(|| "size out of range".to_string());
    if let Some((lo, hi)) = v.split_once("..") {
        return Ok(Cmp::Range(parse_bytes(lo)?, after(parse_bytes(hi)?)?));
    }
    let (ctor, rest): (fn(i64) -> Cmp, &str) = if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else {
        let exact = parse_bytes(v.trim_start_matches('='))?;
        return Ok(Cmp::Range(exact, after(exact)?));
    };
    Ok(ctor(parse_bytes(rest)?))
}

fn parse_bytes(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let mult: f64 = match unit.trim() {
        "" | "b" | "bytes" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024f64.powi(4),
        u => return Err(format!("unknown size unit `{u}` (use kb, mb, gb, tb)")),
    };
    let bytes = n * mult;
    // `as` would saturate silently; `i64::MAX as f64` already rounds up to 2^63.
    if !bytes.is_finite() || bytes >= i64::MAX as f64 {
        return Err(format!("size `{s}` out of range"));
    }
    Ok(bytes as i64)
}

fn parse_date_cmp(value: &str) -> Result<Cmp, String> {
    let v = value.trim().to_ascii_lowercase();
    let today = local_midnight_today();
    match v.as_str() {
        "today" => return Ok(Cmp::Ge(today)),
        "yesterday" => return Ok(Cmp::Range(today - TICKS_PER_DAY, today)),
        _ => {}
    }
    if let Some((lo, hi)) = v.split_once("..") {
        return Ok(Cmp::Range(day_start(lo)?, day_start(hi)? + TICKS_PER_DAY));
    }
    for (prefix, ctor) in [(">=", Cmp::Ge as fn(i64) -> Cmp), ("<=", Cmp::Le), (">", Cmp::Gt), ("<", Cmp::Lt)] {
        if let Some(rest) = v.strip_prefix(prefix) {
            let start = day_start(rest)?;
            // `>date` means after that whole day, `<=date` includes it.
            return Ok(match prefix {
                ">" => Cmp::Ge(start + TICKS_PER_DAY),
                "<=" => Cmp::Lt(start + TICKS_PER_DAY),
                _ => ctor(start),
            });
        }
    }
    if let Some(day) = parse_date(&v) {
        let start = local_midnight(day.0, day.1, day.2);
        return Ok(Cmp::Range(start, start + TICKS_PER_DAY));
    }
    // Relative: last N hours/days/weeks/years.
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: i64 = num.parse().map_err(|_| format!("invalid date `{value}` (try today, 7d or 2026-01-31)"))?;
    let secs = match unit {
        "min" | "mins" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 7 * 86_400,
        "y" | "year" | "years" => 365 * 86_400,
        _ => return Err(format!("unknown date unit `{unit}` (use min, h, d, w, y)")),
    };
    let span = n
        .checked_mul(secs)
        .and_then(|s| s.checked_mul(TICKS_PER_SEC))
        .ok_or_else(|| format!("date interval `{value}` out of range"))?;
    // A span reaching back before 1601 (FILETIME zero) simply means "any date".
    Ok(Cmp::Ge(now_filetime().saturating_sub(span).max(0)))
}

fn day_start(s: &str) -> Result<i64, String> {
    let (y, m, d) = parse_date(s.trim()).ok_or_else(|| format!("invalid date `{s}` (use YYYY-MM-DD)"))?;
    Ok(local_midnight(y, m, d))
}

// ─── Matching ───────────────────────────────────────────────────────────────

/// Per-thread scratch buffers so full paths are built at most once per entry.
#[derive(Default)]
struct Scratch {
    path: String,
    path_for: Option<(u32, bool)>,
}

impl Scratch {
    fn path<'a>(&'a mut self, v: &VolumeIndex, idx: u32, lowercase: bool) -> &'a str {
        if self.path_for != Some((idx, lowercase)) {
            self.path.clear();
            v.write_path(idx, &mut self.path, lowercase);
            self.path_for = Some((idx, lowercase));
        }
        &self.path
    }
}

impl Query {
    fn matches(&self, v: &VolumeIndex, idx: u32, scratch: &mut Scratch) -> bool {
        self.clauses
            .iter()
            .all(|c| c.alts.iter().any(|t| self.test(t, v, idx, scratch)) != c.negate)
    }

    fn test(&self, test: &Test, v: &VolumeIndex, idx: u32, scratch: &mut Scratch) -> bool {
        match test {
            Test::Text { finder, path } => {
                let hay = if *path {
                    scratch.path(v, idx, !self.case)
                } else if self.case {
                    v.name(idx)
                } else {
                    v.lower_name(idx)
                };
                finder.find(hay.as_bytes()).is_some()
            }
            Test::Wildcard { glob, path } => {
                if *path {
                    glob.is_match(scratch.path(v, idx, false))
                } else {
                    glob.is_match(v.name(idx))
                }
            }
            Test::Regex { re, path } => {
                if *path {
                    re.is_match(scratch.path(v, idx, false))
                } else {
                    re.is_match(v.name(idx))
                }
            }
            Test::Ext(exts) => {
                if v.entry(idx).is_dir() {
                    return false;
                }
                let name = v.lower_name(idx);
                match name.rfind('.') {
                    Some(dot) if dot > 0 => exts.iter().any(|e| *e == name[dot + 1..]),
                    _ => exts.iter().any(|e| e.is_empty()),
                }
            }
            Test::Folder => v.entry(idx).is_dir(),
            Test::File => !v.entry(idx).is_dir(),
            Test::Size(cmp) => {
                let e = v.entry(idx);
                !e.is_dir() && cmp.matches(e.size.min(i64::MAX as u64) as i64)
            }
            Test::Modified(cmp) => cmp.matches(v.entry(idx).mtime),
        }
    }
}

const CANCEL_CHECK: usize = 1 << 15;

/// Filters one volume, keeping the order of `order`. Returns `None` when cancelled.
fn filter_volume(v: &VolumeIndex, order: &[u32], q: &Query, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
    if q.is_empty() {
        return Some(order.to_vec());
    }
    let threads = thread_count();
    if order.len() < 50_000 || threads == 1 {
        let mut scratch = Scratch::default();
        let mut out = Vec::new();
        for (n, &idx) in order.iter().enumerate() {
            if n % CANCEL_CHECK == 0 && cancelled() {
                return None;
            }
            if q.matches(v, idx, &mut scratch) {
                out.push(idx);
            }
        }
        return Some(out);
    }

    let chunk = order.len().div_ceil(threads);
    let parts: Vec<Option<Vec<u32>>> = std::thread::scope(|s| {
        let handles: Vec<_> = order
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    let mut scratch = Scratch::default();
                    let mut out = Vec::new();
                    for (n, &idx) in part.iter().enumerate() {
                        if n % CANCEL_CHECK == 0 && cancelled() {
                            return None;
                        }
                        if q.matches(v, idx, &mut scratch) {
                            out.push(idx);
                        }
                    }
                    Some(out)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let mut out = Vec::with_capacity(parts.iter().flatten().map(Vec::len).sum());
    for part in parts {
        out.extend(part?);
    }
    Some(out)
}

/// Runs a query over every volume and merges the per-volume sorted results.
pub fn search(index: &Index, q: &Query, sort: Sort, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<Hit>> {
    let per_volume: Vec<Option<Vec<u32>>> = std::thread::scope(|s| {
        let handles: Vec<_> = index
            .volumes
            .iter()
            .map(|v| s.spawn(move || filter_volume(v, v.order(sort.key), q, cancelled)))
            .collect();
        handles.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let lists: Vec<Vec<u32>> = per_volume.into_iter().collect::<Option<_>>()?;

    let mut hits = merge(index, &lists, sort.key);
    if sort.descending {
        hits.reverse();
    }
    Some(hits)
}

fn merge(index: &Index, lists: &[Vec<u32>], key: SortKey) -> Vec<Hit> {
    let total = lists.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    if key == SortKey::Path || lists.len() == 1 {
        // Volumes are already in drive-letter order, which is path order.
        for (vol, list) in lists.iter().enumerate() {
            out.extend(list.iter().map(|&idx| Hit { vol: vol as u8, idx }));
        }
        return out;
    }

    let cmp = |a: Hit, b: Hit| -> Ordering {
        let (va, vb) = (index.volume(a.vol), index.volume(b.vol));
        let ord = match key {
            SortKey::Name => va.lower_name(a.idx).cmp(vb.lower_name(b.idx)),
            SortKey::Size => va.entry(a.idx).size.cmp(&vb.entry(b.idx).size),
            SortKey::Modified => va.entry(a.idx).mtime.cmp(&vb.entry(b.idx).mtime),
            SortKey::Path => Ordering::Equal,
        };
        ord.then(a.vol.cmp(&b.vol))
    };

    // Merge pairs of runs in parallel rounds: each element is compared about log2(volumes) times.
    let mut runs: Vec<Vec<Hit>> = lists
        .iter()
        .enumerate()
        .map(|(vol, list)| list.iter().map(|&idx| Hit { vol: vol as u8, idx }).collect())
        .collect();
    while runs.len() > 1 {
        let mut pairs = Vec::with_capacity(runs.len().div_ceil(2));
        let mut iter = runs.into_iter();
        while let Some(a) = iter.next() {
            pairs.push((a, iter.next()));
        }
        runs = std::thread::scope(|s| {
            let handles: Vec<_> = pairs
                .into_iter()
                .map(|(a, b)| {
                    let cmp = &cmp;
                    s.spawn(move || match b {
                        Some(b) => merge_two(a, b, cmp),
                        None => a,
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("merge thread")).collect()
        });
    }
    out.extend(runs.pop().unwrap_or_default());
    out
}

fn merge_two(a: Vec<Hit>, b: Vec<Hit>, cmp: &impl Fn(Hit, Hit) -> Ordering) -> Vec<Hit> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if cmp(a[i], b[j]) != Ordering::Greater {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

// ─── Background searcher ────────────────────────────────────────────────────

pub struct Request {
    pub generation: u64,
    pub index: Index,
    pub text: String,
    pub options: SearchOptions,
    pub sort: Sort,
}

pub struct Response {
    pub generation: u64,
    /// The snapshot the hits refer to.
    pub index: Index,
    pub hits: Arc<Vec<Hit>>,
    pub error: Option<String>,
    pub elapsed: Duration,
}

/// Runs searches on a background thread; newer requests cancel older ones.
pub struct Searcher {
    tx: Sender<Request>,
    pub results: Receiver<Response>,
    latest: Arc<AtomicU64>,
    next_generation: u64,
}

impl Searcher {
    pub fn new(on_result: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = unbounded::<Request>();
        let (res_tx, res_rx) = unbounded::<Response>();
        let latest = Arc::new(AtomicU64::new(0));
        let latest_bg = Arc::clone(&latest);

        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                // Unfiltered result lists are large and expensive to merge; reuse them while the
                // snapshot is unchanged (e.g. when the query is cleared again).
                let mut everything: Vec<(Vec<usize>, Sort, Arc<Vec<Hit>>)> = Vec::new();
                while let Ok(mut req) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        req = newer;
                    }
                    let start = Instant::now();
                    let generation = req.generation;
                    let cancelled = || latest_bg.load(AtomicOrdering::Relaxed) != generation;
                    let snapshot: Vec<usize> = req.index.volumes.iter().map(|v| Arc::as_ptr(v) as usize).collect();
                    everything.retain(|(s, _, _)| *s == snapshot);

                    let (hits, error) = match Query::parse(&req.text, req.options) {
                        Ok(q) if q.is_empty() => {
                            match everything.iter().find(|(_, sort, _)| *sort == req.sort) {
                                Some((_, _, hits)) => (Arc::clone(hits), None),
                                None => match search(&req.index, &q, req.sort, &cancelled) {
                                    Some(h) => {
                                        let h = Arc::new(h);
                                        everything.push((snapshot, req.sort, Arc::clone(&h)));
                                        (h, None)
                                    }
                                    None => continue,
                                },
                            }
                        }
                        Ok(q) => match search(&req.index, &q, req.sort, &cancelled) {
                            Some(h) => (Arc::new(h), None),
                            None => continue,
                        },
                        Err(e) => (Arc::new(Vec::new()), Some(e)),
                    };
                    let response = Response {
                        generation,
                        index: req.index,
                        hits,
                        error,
                        elapsed: start.elapsed(),
                    };
                    if res_tx.send(response).is_err() {
                        break;
                    }
                    on_result();
                }
            })
            .expect("spawn search thread");

        Searcher { tx, results: res_rx, latest, next_generation: 0 }
    }

    pub fn submit(&mut self, index: Index, text: &str, options: SearchOptions, sort: Sort) -> u64 {
        self.next_generation += 1;
        let generation = self.next_generation;
        self.latest.store(generation, AtomicOrdering::Relaxed);
        let _ = self.tx.send(Request { generation, index, text: text.to_string(), options, sort });
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn run(text: &str, opts: SearchOptions, sort: Sort) -> Result<Vec<String>, String> {
        let index = Index::default().with_volume(Arc::new(sample()));
        let q = Query::parse(text, opts)?;
        let hits = search(&index, &q, sort, &|| false).unwrap();
        Ok(hits.iter().map(|h| index.volume(h.vol).name(h.idx).to_string()).collect())
    }

    fn names(text: &str) -> Vec<String> {
        run(text, SearchOptions::default(), Sort::default()).unwrap()
    }

    #[test]
    fn text_terms_negation_and_or() {
        assert_eq!(names(""), ["big.bin", "docs", "main.rs", "Notes.TXT", "readme.md", "src"]);
        assert_eq!(names("NOTES"), ["Notes.TXT"]);
        assert_eq!(names("e m"), ["readme.md"]);
        assert_eq!(names("!s"), ["big.bin", "readme.md"]);
        assert_eq!(names("main|big"), ["big.bin", "main.rs"]);
        assert_eq!(names("\"me.m\""), ["readme.md"]);
    }

    #[test]
    fn match_case_option() {
        let opts = SearchOptions { match_case: true, ..Default::default() };
        assert_eq!(run("notes", opts, Sort::default()).unwrap(), Vec::<String>::new());
        assert_eq!(run("Notes", opts, Sort::default()).unwrap(), ["Notes.TXT"]);
    }

    #[test]
    fn wildcards_paths_and_prefixes() {
        assert_eq!(names("*.rs"), ["main.rs"]);
        assert_eq!(names("*.r"), Vec::<String>::new());
        assert_eq!(names("docs\\"), ["Notes.TXT", "readme.md"]);
        assert_eq!(names("c:/src/"), ["main.rs"]);
        assert_eq!(names("ext:md;txt"), ["Notes.TXT", "readme.md"]);
        assert_eq!(names("folder:"), ["docs", "src"]);
        assert_eq!(names("file: m"), ["main.rs", "readme.md"]);
        assert_eq!(names("folder:s"), ["docs", "src"]);
        assert_eq!(names("folder:oc"), ["docs"]);
        assert_eq!(names("!folder:"), names("file:"));
        assert_eq!(names("size:>200"), ["big.bin", "main.rs"]);
        assert_eq!(names("size:50..100"), ["Notes.TXT", "readme.md"]);
        assert_eq!(names("regex:^[a-m].*\\.\\w+$"), ["big.bin", "main.rs"]);
        assert!(Query::parse("size:>10zb", SearchOptions::default()).is_err());
        assert!(Query::parse("regex:(", SearchOptions::default()).is_err());
    }

    /// FC-09: out-of-range numbers are errors instead of panics (debug) or wrapped bounds (release).
    #[test]
    fn numeric_overflow_is_rejected() {
        let parse = |q: &str| Query::parse(q, SearchOptions::default());
        for q in [
            "size:9223372036854775807",
            "size:..9223372036854775807",
            "size:>99999999tb",
            "size:<9999999999999999999999",
            "dm:9223372036854775807y",
            "dm:999999999999999h",
        ] {
            assert!(parse(q).is_err(), "{q} was accepted");
        }
        // Large but representable values still work.
        assert_eq!(names("size:<8000000tb"), names("file:"));
        assert_eq!(names("dm:1000y"), names(""), "a span before 1601 matches every date");
        assert!(parse("size:9223372036854774784").is_ok());
    }

    #[test]
    fn regex_mode_and_match_path() {
        let opts = SearchOptions { regex: true, ..Default::default() };
        assert_eq!(run("^(main|big)\\.", opts, Sort::default()).unwrap(), ["big.bin", "main.rs"]);
        let opts = SearchOptions { match_path: true, ..Default::default() };
        assert_eq!(run("src", opts, Sort::default()).unwrap(), ["main.rs", "src"]);
    }

    #[test]
    fn sorting_and_merging_across_volumes() {
        let size_desc = Sort { key: SortKey::Size, descending: true };
        assert_eq!(run("file:", SearchOptions::default(), size_desc).unwrap(), ["big.bin", "main.rs", "readme.md", "Notes.TXT"]);

        // Two volumes: names interleave in name order.
        let mut b = crate::index::VolumeBuilder::new('D');
        b.push("apple", 0, 0, 1, 1);
        b.push("zebra", 0, 0, 1, 1);
        let d = b.finish(crate::index::Source::Walk, Duration::ZERO);
        let index = Index::default().with_volume(Arc::new(sample())).with_volume(Arc::new(d));
        let q = Query::parse("file:", SearchOptions::default()).unwrap();
        let hits = search(&index, &q, Sort::default(), &|| false).unwrap();
        let got: Vec<_> = hits.iter().map(|h| index.volume(h.vol).name(h.idx)).collect();
        assert_eq!(got, ["apple", "big.bin", "main.rs", "Notes.TXT", "readme.md", "zebra"]);
    }
}
