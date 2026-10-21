//! Discovers drives and keeps one [`VolumeIndex`] per drive up to date in the background:
//! load the on-disk cache first (instant), then rebuild via the MFT (admin) or a folder walk.

use crate::index::{Source, VolumeIndex, cache_path};
use crate::{mft, walk};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug)]
pub struct Drive {
    pub letter: char,
    pub filesystem: String,
    pub label: String,
}

impl Drive {
    pub fn is_ntfs(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("NTFS")
    }
}

#[derive(Clone, Debug)]
pub enum Phase {
    Waiting,
    Indexing(Source),
    Ready,
    Failed(String),
}

#[derive(Clone)]
pub struct VolumeStatus {
    pub drive: Drive,
    pub phase: Phase,
    pub progress: Arc<AtomicUsize>,
    /// Expected total for progress (MFT records); 0 when unknown.
    pub total: Arc<AtomicUsize>,
    pub entries: usize,
    pub source: Option<Source>,
    pub built_at: Option<SystemTime>,
    pub build_time: Option<Duration>,
    /// Shown while a rebuild of a cached index is running.
    pub refreshing: bool,
}

pub enum IndexEvent {
    Ready(Arc<VolumeIndex>),
}

pub struct Indexer {
    pub status: Arc<Mutex<BTreeMap<char, VolumeStatus>>>,
    pub events: Receiver<IndexEvent>,
    tx: Sender<IndexEvent>,
    /// Cancel flag of the running job for each drive.
    cancels: BTreeMap<char, Arc<AtomicBool>>,
    pub elevated: bool,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Indexer {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = unbounded();
        Indexer {
            status: Arc::new(Mutex::new(BTreeMap::new())),
            events: rx,
            tx,
            cancels: BTreeMap::new(),
            elevated: is_elevated(),
            notify: Arc::new(notify),
        }
    }

    /// Stops indexing a drive and forgets its status.
    pub fn remove(&mut self, letter: char) {
        if let Some(c) = self.cancels.remove(&letter) {
            c.store(true, Ordering::Relaxed);
        }
        self.status.lock().unwrap().remove(&letter);
    }

    /// (Re)indexes `drives`, cancelling any job already running for them. Cached indexes younger
    /// than `max_age` are used as-is unless `force`.
    pub fn start(&mut self, drives: Vec<Drive>, max_age: Duration, force: bool) {
        let mut status = self.status.lock().unwrap();
        for drive in drives {
            let cancel = Arc::new(AtomicBool::new(false));
            if let Some(old) = self.cancels.insert(drive.letter, Arc::clone(&cancel)) {
                old.store(true, Ordering::Relaxed);
            }
            let previous = status.get(&drive.letter).cloned();
            let st = VolumeStatus {
                drive: drive.clone(),
                phase: if previous.as_ref().is_some_and(|p| matches!(p.phase, Phase::Ready)) { Phase::Ready } else { Phase::Waiting },
                progress: Arc::new(AtomicUsize::new(0)),
                total: Arc::new(AtomicUsize::new(0)),
                entries: previous.as_ref().map_or(0, |p| p.entries),
                source: previous.as_ref().and_then(|p| p.source),
                built_at: previous.as_ref().and_then(|p| p.built_at),
                build_time: previous.as_ref().and_then(|p| p.build_time),
                refreshing: false,
            };
            status.insert(drive.letter, st.clone());

            let job = Job {
                drive,
                status: Arc::clone(&self.status),
                tx: self.tx.clone(),
                cancel,
                elevated: self.elevated,
                notify: Arc::clone(&self.notify),
                progress: st.progress,
                total: st.total,
                max_age,
                force,
                has_index: previous.is_some_and(|p| matches!(p.phase, Phase::Ready)),
            };
            let _ = std::thread::Builder::new()
                .name(format!("index {}", job.drive.letter))
                .spawn(move || job.run());
        }
    }

    pub fn is_busy(&self) -> bool {
        self.status
            .lock()
            .unwrap()
            .values()
            .any(|s| matches!(s.phase, Phase::Waiting | Phase::Indexing(_)) || s.refreshing)
    }
}

struct Job {
    drive: Drive,
    status: Arc<Mutex<BTreeMap<char, VolumeStatus>>>,
    tx: Sender<IndexEvent>,
    cancel: Arc<AtomicBool>,
    elevated: bool,
    notify: Arc<dyn Fn() + Send + Sync>,
    progress: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    max_age: Duration,
    force: bool,
    /// The UI already holds an index for this drive (from an earlier pass).
    has_index: bool,
}

impl Job {
    fn update(&self, f: impl FnOnce(&mut VolumeStatus)) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        if let Some(s) = self.status.lock().unwrap().get_mut(&self.drive.letter) {
            f(s);
        }
        (self.notify)();
    }

    fn publish(&self, v: VolumeIndex) {
        let (entries, source, built_at, build_time) = (v.len(), v.source, v.built_at, v.build_time);
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.tx.send(IndexEvent::Ready(Arc::new(v)));
        self.update(|s| {
            s.phase = Phase::Ready;
            s.entries = entries;
            s.source = Some(source);
            s.built_at = Some(built_at);
            s.build_time = Some(build_time);
        });
    }

    fn run(self) {
        let letter = self.drive.letter;
        let mut have_index = self.has_index;
        let mut fresh = false;

        if !self.has_index
            && let Ok(cached) = VolumeIndex::load(&cache_path(letter), letter) {
                let age = cached.built_at.elapsed().unwrap_or(Duration::MAX);
                fresh = age < self.max_age;
                self.publish(cached);
                have_index = true;
            }
        if fresh && !self.force {
            return;
        }

        let use_mft = self.elevated && self.drive.is_ntfs();
        self.update(|s| {
            if have_index {
                s.refreshing = true;
            } else {
                s.phase = Phase::Indexing(if use_mft { Source::Mft } else { Source::Walk });
            }
        });

        let mut result = if use_mft {
            mft::read_volume(letter, &self.progress, &self.total, &self.cancel)
        } else {
            Err(anyhow::anyhow!("not elevated"))
        };
        if result.is_err() && !self.cancel.load(Ordering::Relaxed) {
            self.total.store(0, Ordering::Relaxed);
            self.update(|s| {
                if !have_index {
                    s.phase = Phase::Indexing(Source::Walk);
                }
            });
            let root = PathBuf::from(format!("{letter}:\\"));
            result = walk::walk_volume(letter, &root, &self.progress, &self.cancel);
        }

        match result {
            Ok(v) => {
                let _ = v.save(&cache_path(letter));
                self.publish(v);
                self.update(|s| s.refreshing = false);
            }
            Err(e) => self.update(|s| {
                s.refreshing = false;
                if !have_index {
                    s.phase = Phase::Failed(e.to_string());
                }
            }),
        }
    }
}

// ─── Windows queries ────────────────────────────────────────────────────────

#[cfg(windows)]
pub fn is_elevated() -> bool {
    // SAFETY: no arguments; returns a BOOL.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

#[cfg(windows)]
pub fn fixed_drives() -> Vec<Drive> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    const DRIVE_FIXED: u32 = 3;

    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        // SAFETY: `root` is a NUL-terminated UTF-16 string.
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        // SAFETY: buffers are valid for the lengths passed.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        let text = |buf: &[u16]| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        };
        drives.push(Drive {
            letter,
            filesystem: if ok != 0 { text(&fs) } else { String::new() },
            label: if ok != 0 { text(&label) } else { String::new() },
        });
    }
    drives
}

#[cfg(not(windows))]
pub fn fixed_drives() -> Vec<Drive> {
    Vec::new()
}
