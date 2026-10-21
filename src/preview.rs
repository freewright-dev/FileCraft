//! Preview pane content and in-place text editing.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use crossbeam_channel::{Receiver, Sender, unbounded};

/// Bytes shown when previewing (not editing) a text file.
const PREVIEW_BYTES: usize = 512 * 1024;
/// Largest file that can be edited inline.
pub const EDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DIR_LIMIT: usize = 1000;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "ico", "webp"];

pub enum Content {
    Text {
        text: String,
        /// Only the first [`PREVIEW_BYTES`] were read.
        truncated: bool,
    },
    Image(String),
    Binary(String),
    Directory { entries: Vec<(bool, String)>, total: usize },
    Message(String),
}

pub enum SaveError {
    /// The file was modified, replaced or deleted on disk after it was loaded.
    Conflict,
    Io(String),
}

/// Which file was loaded and what it contained; a save compares this with the file on disk.
/// Modification times are not enough: they can be preserved or reset by other programs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileVersion {
    /// Volume and file index (device and inode elsewhere): a replaced file gets a new one.
    id: (u64, u64),
    hash: u64,
}

impl FileVersion {
    fn of(file: &fs::File, bytes: &[u8]) -> io::Result<FileVersion> {
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Ok(FileVersion { id: file_id(file)?, hash: hasher.finish() })
    }
}

fn file_id(file: &fs::File) -> io::Result<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        // SAFETY: zero is a valid initial state for this plain struct.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle stays open for the call and `info` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(info.dwVolumeSerialNumber), u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow)))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
}

pub struct Preview {
    pub path: PathBuf,
    pub content: Content,
    pub editing: bool,
    /// Edit buffer with `\n` line endings.
    pub buffer: String,
    pub dirty: bool,
    pub conflict: bool,
    /// Why the file cannot be edited inline, if it cannot.
    pub not_editable: Option<String>,
    /// The complete file as it was read; `None` until the whole file has been read.
    loaded: Option<FileVersion>,
    crlf: bool,
    bom: bool,
}

/// Serial background I/O: rapid selection changes do not spawn unbounded readers.
pub struct PreviewLoader {
    requests: Sender<(u64, PathBuf, bool)>,
    results: Receiver<(u64, Preview)>,
    generation: u64,
}

impl PreviewLoader {
    pub fn new(on_result: impl Fn() + Send + 'static) -> Self {
        let (requests, rx) = unbounded::<(u64, PathBuf, bool)>();
        let (tx, results) = unbounded();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                while let Ok(mut request) = rx.recv() {
                    // Skip selections made while the previous read was in progress.
                    while let Ok(newer) = rx.try_recv() {
                        request = newer;
                    }
                    let (generation, path, is_dir) = request;
                    let preview = Preview::load(&path, is_dir);
                    if tx.send((generation, preview)).is_err() {
                        break;
                    }
                    on_result();
                }
            })
            .expect("spawn preview thread");
        Self { requests, results, generation: 0 }
    }

    pub fn submit(&mut self, path: PathBuf, is_dir: bool) {
        self.generation += 1;
        let _ = self.requests.send((self.generation, path, is_dir));
    }

    /// Only the most recently requested preview may replace the pane.
    pub fn poll(&self) -> Option<Preview> {
        while let Ok((generation, preview)) = self.results.try_recv() {
            if generation == self.generation {
                return Some(preview);
            }
        }
        None
    }
}

impl Preview {
    pub fn load(path: &Path, is_dir: bool) -> Preview {
        let mut p = Preview {
            path: path.to_path_buf(),
            content: Content::Message(String::new()),
            editing: false,
            buffer: String::new(),
            dirty: false,
            conflict: false,
            not_editable: None,
            loaded: None,
            crlf: false,
            bom: false,
        };
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                p.not_editable = Some("file not readable".into());
                p.content = Content::Message(format!("Cannot read file: {e}"));
                return p;
            }
        };
        // Check before reading content, enumerating folders, or handing images to egui.
        // Cloud placeholders must not be downloaded just because a row was selected.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS};
            if meta.file_attributes() & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0 {
                p.not_editable = Some("not available locally".into());
                p.content = Content::Message(
                    "Not fully available on this device. Preview skipped to avoid a cloud download. Download it in your sync app, then select another result and return.".into(),
                );
                return p;
            }
        }
        p.content = if is_dir { load_dir(path) } else { p.load_file(&meta) };
        p
    }

    fn load_file(&mut self, meta: &fs::Metadata) -> Content {
        let ext = self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            self.not_editable = Some("image".into());
            return Content::Image(format!("file://{}", self.path.display()));
        }

        let mut buf = Vec::with_capacity(PREVIEW_BYTES.min(meta.len() as usize));
        let file = match fs::File::open(&self.path).and_then(|f| (&f).take(PREVIEW_BYTES as u64).read_to_end(&mut buf).map(|_| f)) {
            Ok(f) => f,
            Err(e) => {
                self.not_editable = Some("file not readable".into());
                return Content::Message(format!("Cannot read file: {e}"));
            }
        };
        if buf.iter().take(8192).any(|&b| b == 0) {
            self.not_editable = Some("binary file".into());
            return Content::Binary(hex_dump(&buf[..buf.len().min(4096)]));
        }

        let truncated = (buf.len() as u64) < meta.len();
        if !truncated {
            self.loaded = FileVersion::of(&file, &buf).ok();
        }
        if meta.len() > EDIT_MAX_BYTES {
            self.not_editable = Some(format!("larger than {} MB", EDIT_MAX_BYTES / (1024 * 1024)));
        }
        let (text, valid) = self.decode(&buf, truncated);
        if !valid {
            self.not_editable = Some("not UTF-8 text".into());
        }
        Content::Text { text, truncated }
    }

    /// Decodes bytes as UTF-8 (lossy for display); records BOM and line-ending style.
    fn decode(&mut self, bytes: &[u8], truncated: bool) -> (String, bool) {
        let (bom, body) = match bytes.strip_prefix(b"\xEF\xBB\xBF") {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        self.bom = bom;
        let (text, valid) = match std::str::from_utf8(body) {
            Ok(s) => (s.to_string(), true),
            // A truncated preview may cut a multi-byte character in half.
            Err(e) if truncated && e.error_len().is_none() => (String::from_utf8_lossy(&body[..e.valid_up_to()]).into_owned(), true),
            Err(_) => (String::from_utf8_lossy(body).into_owned(), false),
        };
        self.crlf = text.contains("\r\n");
        (if self.crlf { text.replace("\r\n", "\n") } else { text }, valid)
    }

    pub fn can_edit(&self) -> bool {
        matches!(self.content, Content::Text { .. }) && self.not_editable.is_none()
    }

    pub fn begin_edit(&mut self) -> Result<(), String> {
        // Already editing (context menu or F4 again): keep the buffer; only Reload discards it.
        if self.editing {
            return Ok(());
        }
        if let Some(reason) = &self.not_editable {
            return Err(format!("Cannot edit inline: {reason}"));
        }
        let Content::Text { text, truncated } = &self.content else {
            return Err("Only text files can be edited inline".into());
        };
        self.buffer = if *truncated {
            let mut bytes = Vec::new();
            let mut file = fs::File::open(&self.path).map_err(|e| e.to_string())?;
            file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            self.loaded = Some(FileVersion::of(&file, &bytes).map_err(|e| e.to_string())?);
            let (text, valid) = self.decode(&bytes, false);
            if !valid {
                return Err("Cannot edit inline: not UTF-8 text".into());
            }
            text
        } else {
            text.clone()
        };
        self.editing = true;
        self.dirty = false;
        self.conflict = false;
        Ok(())
    }

    pub fn cancel_edit(&mut self) {
        self.editing = false;
        self.dirty = false;
        self.conflict = false;
        self.buffer.clear();
    }

    /// Writes the edit buffer. Refuses when the file was changed, replaced or deleted on disk
    /// since it was loaded, unless `force`. The file is checked and written through one handle,
    /// so swapping the path (rename, link, junction) in between cannot redirect the write.
    pub fn save(&mut self, force: bool) -> Result<(), SaveError> {
        let io_err = |e: io::Error| SaveError::Io(e.to_string());
        let mut out = Vec::with_capacity(self.buffer.len() + 3);
        if self.bom {
            out.extend_from_slice(b"\xEF\xBB\xBF");
        }
        if self.crlf {
            out.extend_from_slice(self.buffer.replace('\n', "\r\n").as_bytes());
        } else {
            out.extend_from_slice(self.buffer.as_bytes());
        }

        let file = match open_for_save(&self.path) {
            Ok(mut file) => {
                let mut original = Vec::new();
                file.read_to_end(&mut original).map_err(io_err)?;
                if !force && self.loaded != Some(FileVersion::of(&file, &original).map_err(io_err)?) {
                    self.conflict = true;
                    return Err(SaveError::Conflict);
                }
                let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                write_in_place(&mut file, &name, &original, &out, &backup_dir()).map_err(io_err)?;
                file
            }
            // Deleted on disk: only an explicit Overwrite recreates it.
            Err(e) if e.kind() == io::ErrorKind::NotFound && !force => {
                self.conflict = true;
                return Err(SaveError::Conflict);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut file = fs::File::options().write(true).create_new(true).open(&self.path).map_err(io_err)?;
                file.write_all(&out).and_then(|()| file.sync_all()).map_err(io_err)?;
                file
            }
            Err(e) => return Err(io_err(e)),
        };

        self.loaded = Some(FileVersion::of(&file, &out).map_err(io_err)?);
        self.dirty = false;
        self.conflict = false;
        let truncated = out.len() > PREVIEW_BYTES;
        let shown = if truncated { truncate_at_char(&self.buffer, PREVIEW_BYTES) } else { self.buffer.clone() };
        self.content = Content::Text { text: shown, truncated };
        Ok(())
    }

}

/// Opens the existing file for a save. Other programs may still read it, but on Windows they
/// cannot write, rename or delete it until the save is done.
fn open_for_save(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::File::options();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

/// Where a save keeps a copy of the original until the new content is completely written.
fn backup_dir() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join(format!("filecraft-backup-{}", std::process::id()))
    } else {
        crate::index::data_dir().join("backup")
    }
}

/// Overwrites `file` with `new` through its open handle, so permissions, attributes and hard
/// links are kept. `old` is first copied to `backup_dir`; the copy is deleted once the write
/// completes or `old` has been written back after a failure. Otherwise the error names it.
fn write_in_place(file: &mut fs::File, name: &str, old: &[u8], new: &[u8], backup_dir: &Path) -> io::Result<()> {
    let backup = write_backup(backup_dir, name, old)?;
    match overwrite(file, new) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(e) if overwrite(file, old).is_ok() => {
            let _ = fs::remove_file(&backup);
            Err(e)
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("{e} (the original is saved as {})", backup.display()))),
    }
}

fn write_backup(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(dir)?;
    loop {
        let path = dir.join(format!("{}-{}-{name}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = match fs::File::options().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        return Ok(path);
    }
}

fn overwrite(file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
    let len = bytes.len() as u64;
    // Grow first, so running out of disk space fails before any byte is replaced.
    if len > file.metadata()?.len() {
        file.set_len(len)?;
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(len)?;
    file.sync_all()
}

fn truncate_at_char(s: &str, max: usize) -> String {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn load_dir(path: &Path) -> Content {
    match fs::read_dir(path) {
        Ok(rd) => {
            let mut entries: Vec<(bool, String)> = rd
                .flatten()
                .map(|e| (e.file_type().is_ok_and(|t| t.is_dir()), e.file_name().to_string_lossy().into_owned()))
                .collect();
            entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
            let total = entries.len();
            entries.truncate(DIR_LIMIT);
            Content::Directory { entries, total }
        }
        Err(e) => Content::Message(format!("Cannot read folder: {e}")),
    }
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", row * 16));
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        out.push_str(&"   ".repeat(16 - chunk.len()));
        out.push(' ');
        out.extend(chunk.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filecraft-preview-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn edits_preserve_bom_and_crlf() {
        let path = temp("crlf.txt", b"\xEF\xBB\xBFone\r\ntwo\r\n");
        let mut p = Preview::load(&path, false);
        assert!(matches!(&p.content, Content::Text { text, .. } if text == "one\ntwo\n"));
        p.begin_edit().unwrap();
        p.buffer.push_str("three\n");
        p.dirty = true;
        assert!(p.save(false).is_ok());
        assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFone\r\ntwo\r\nthree\r\n");
        assert!(!p.dirty);
    }

    /// FC-03: starting an edit again on the same file must not reset unsaved work.
    #[test]
    fn re_entering_edit_keeps_unsaved_work() {
        let path = temp("reenter.txt", b"original");
        let mut p = Preview::load(&path, false);
        p.begin_edit().unwrap();
        p.buffer = "unsaved work".into();
        p.dirty = true;
        p.begin_edit().unwrap();
        assert_eq!(p.buffer, "unsaved work");
        assert!(p.dirty && p.editing);
        p.cancel_edit();
        p.begin_edit().unwrap();
        assert_eq!(p.buffer, "original");
        assert!(!p.dirty);
    }

    #[test]
    fn detects_conflicting_changes_on_disk() {
        let path = temp("conflict.txt", b"a\n");
        let mut p = Preview::load(&path, false);
        p.begin_edit().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, b"changed elsewhere\n").unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        p.buffer = "mine\n".into();
        assert!(matches!(p.save(false), Err(SaveError::Conflict)));
        assert!(p.save(true).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
    }

    /// FC-04: a file deleted on disk is a conflict; only Overwrite recreates it.
    #[test]
    fn deleted_file_is_a_conflict() {
        let path = temp("deleted.txt", b"original");
        let mut p = Preview::load(&path, false);
        p.begin_edit().unwrap();
        p.buffer = "replacement".into();
        fs::remove_file(&path).unwrap();
        assert!(matches!(p.save(false), Err(SaveError::Conflict)));
        assert!(!path.exists());
        assert!(p.save(true).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    }

    /// FC-04: keeping the modification time does not hide a rewritten or replaced file.
    #[test]
    fn same_mtime_changes_are_conflicts() {
        let path = temp("same-mtime.txt", b"original");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let mut p = Preview::load(&path, false);
        p.begin_edit().unwrap();
        p.buffer = "mine".into();

        fs::write(&path, b"ORIGINAL").unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_modified(mtime).unwrap();
        assert!(matches!(p.save(false), Err(SaveError::Conflict)), "rewritten in place");

        let other = temp("same-mtime-other.txt", b"original");
        fs::File::options().write(true).open(&other).unwrap().set_modified(mtime).unwrap();
        fs::rename(&other, &path).unwrap();
        assert!(matches!(p.save(false), Err(SaveError::Conflict)), "replaced by another file");
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    /// FC-04: when a write fails and the original cannot be put back, a copy of it survives.
    #[test]
    fn failed_write_keeps_a_copy_of_the_original() {
        let path = temp("failed-write.txt", b"original");
        let dir = std::env::temp_dir().join(format!("filecraft-backup-failed-{}", std::process::id()));
        // A read-only handle makes both the write and the restore fail.
        let mut file = fs::File::open(&path).unwrap();
        let err = write_in_place(&mut file, "failed-write.txt", b"original", b"new content", &dir).unwrap_err();
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let backups: Vec<PathBuf> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(backups.len(), 1);
        assert!(err.to_string().contains(&backups[0].display().to_string()), "{err}");
        assert_eq!(fs::read(&backups[0]).unwrap(), b"original");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn successful_saves_leave_no_backup() {
        let path = temp("no-backup.txt", b"before");
        let dir = std::env::temp_dir().join(format!("filecraft-backup-ok-{}", std::process::id()));
        let mut file = open_for_save(&path).unwrap();
        write_in_place(&mut file, "no-backup.txt", b"before", b"after, and longer", &dir).unwrap();
        drop(file);
        assert_eq!(fs::read(&path).unwrap(), b"after, and longer");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn binary_and_images_are_not_editable() {
        let bin = temp("data.bin", &[0, 1, 2, 3, 255]);
        let p = Preview::load(&bin, false);
        assert!(matches!(p.content, Content::Binary(_)));
        assert!(!p.can_edit());
        let img = temp("pic.png", b"not really a png");
        assert!(matches!(Preview::load(&img, false).content, Content::Image(_)));
    }

    #[test]
    fn background_previews_only_deliver_the_latest_selection() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let first = temp("first.txt", b"first preview");
        let last = temp("last.txt", b"last preview");
        let (ready_tx, ready_rx) = unbounded();
        let (release_tx, release_rx) = unbounded();
        let first_result = AtomicBool::new(true);
        let mut loader = PreviewLoader::new(move || {
            ready_tx.send(()).unwrap();
            if first_result.swap(false, Ordering::Relaxed) {
                // Hold the worker while the UI changes selection, without timing races.
                let _ = release_rx.recv();
            }
        });
        loader.submit(first.clone(), false);
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        fs::write(&first, b"updated preview").unwrap();
        loader.submit(last.clone(), false);
        // Returning to the same path must still discard its older result.
        loader.submit(first.clone(), false);
        assert!(loader.poll().is_none());
        release_tx.send(()).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let preview = loader.poll().expect("latest preview");
        assert_eq!(preview.path, first);
        assert!(matches!(preview.content, Content::Text { text, .. } if text == "updated preview"));
        fs::remove_file(first).unwrap();
        fs::remove_file(last).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn offline_items_are_not_read_or_sent_to_image_loaders() {
        use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_OFFLINE, SetFileAttributesW};

        let text = temp("offline.txt", b"local text");
        let image = temp("offline.png", b"image bytes");
        let folder = text.parent().unwrap().join("offline-folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("child.txt"), b"child").unwrap();
        for (path, is_dir) in [(&text, false), (&image, false), (&folder, true)] {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            let attributes = fs::metadata(path).unwrap().file_attributes();
            // SAFETY: NUL-terminated path to a disposable test fixture.
            assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), attributes | FILE_ATTRIBUTE_OFFLINE) }, 0);
            let mut preview = Preview::load(path, is_dir);
            // Restore before assertions so a failed check doesn't leave an offline fixture.
            assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), attributes) }, 0);
            assert!(matches!(&preview.content, Content::Message(msg) if msg.contains("cloud download")));
            assert!(!preview.can_edit());
            assert!(preview.begin_edit().is_err());
        }
        assert!(matches!(Preview::load(&text, false).content, Content::Text { text, .. } if text == "local text"));
        assert!(matches!(Preview::load(&image, false).content, Content::Image(_)));
        assert!(matches!(Preview::load(&folder, true).content, Content::Directory { total: 1, .. }));
        fs::remove_file(text).unwrap();
        fs::remove_file(image).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }
}
