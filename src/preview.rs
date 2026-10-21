//! Preview pane content and in-place text editing.

use std::collections::BinaryHeap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;

/// Bytes shown when previewing (not editing) a text file.
const PREVIEW_BYTES: usize = 512 * 1024;
/// Largest file that can be edited inline.
pub const EDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DIR_LIMIT: usize = 1000;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "ico", "webp"];
/// Largest image file that is previewed.
const IMAGE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Largest image width or height that is decoded.
const IMAGE_MAX_SIDE: u32 = 16_384;
/// Most memory the decoder may allocate (about 64 megapixels as RGBA).
const IMAGE_MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// Decoded images are scaled down to fit this square before they reach the UI.
const IMAGE_PREVIEW_SIDE: u32 = 2048;

pub enum Content {
    Text {
        text: String,
        /// Only the first [`PREVIEW_BYTES`] were read.
        truncated: bool,
    },
    Image(ImagePreview),
    Binary(String),
    Directory { entries: Vec<(bool, String)>, total: usize },
    Message(String),
}

/// An image decoded on the preview thread (first frame only, scaled to fit the pane). The UI
/// turns the pixels into a texture, which is freed together with the preview.
pub struct ImagePreview {
    pub pixels: Option<egui::ColorImage>,
    pub texture: Option<egui::TextureHandle>,
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
    /// The whole text of a truncated preview, read on the preview thread for editing.
    edit_text: Option<String>,
    /// Saves end every line with CRLF (else LF).
    crlf: bool,
    /// The file used both CRLF and LF; saving converts every line to one style.
    mixed_newlines: bool,
    bom: bool,
}

/// A preview request: generation, path, whether it is a folder, whether it is for editing.
type Request = (u64, PathBuf, bool, bool);

/// Serial background I/O: rapid selection changes do not spawn unbounded readers.
pub struct PreviewLoader {
    requests: Sender<Request>,
    results: Receiver<(u64, Preview)>,
    generation: u64,
    /// The newest generation requested, so the worker can abandon a folder listing early.
    latest: Arc<AtomicU64>,
}

impl PreviewLoader {
    pub fn new(on_result: impl Fn() + Send + 'static) -> Self {
        let (requests, rx) = unbounded::<Request>();
        let (tx, results) = unbounded();
        let latest = Arc::new(AtomicU64::new(0));
        let newest = Arc::clone(&latest);
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                while let Ok(mut request) = rx.recv() {
                    // Skip selections made while the previous read was in progress.
                    while let Ok(newer) = rx.try_recv() {
                        request = newer;
                    }
                    let (generation, path, is_dir, edit) = request;
                    let stale = || newest.load(Ordering::Relaxed) != generation;
                    let preview =
                        if edit { Preview::load_for_edit(&path) } else { Preview::load_cancellable(&path, is_dir, &stale) };
                    if tx.send((generation, preview)).is_err() {
                        break;
                    }
                    on_result();
                }
            })
            .expect("spawn preview thread");
        Self { requests, results, generation: 0, latest }
    }

    pub fn submit(&mut self, path: PathBuf, is_dir: bool) {
        self.send(path, is_dir, false);
    }

    /// Requests a file preview that is also ready for editing (see [`Preview::load_for_edit`]).
    pub fn submit_edit(&mut self, path: PathBuf) {
        self.send(path, false, true);
    }

    fn send(&mut self, path: PathBuf, is_dir: bool, edit: bool) {
        self.generation += 1;
        self.latest.store(self.generation, Ordering::Relaxed);
        let _ = self.requests.send((self.generation, path, is_dir, edit));
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
        Preview::load_cancellable(path, is_dir, &|| false)
    }

    /// Like [`Preview::load`]; a folder listing stops early once `stale` returns true.
    pub fn load_cancellable(path: &Path, is_dir: bool, stale: &dyn Fn() -> bool) -> Preview {
        let mut p = Preview {
            path: path.to_path_buf(),
            content: Content::Message(String::new()),
            editing: false,
            buffer: String::new(),
            dirty: false,
            conflict: false,
            not_editable: None,
            loaded: None,
            edit_text: None,
            crlf: false,
            mixed_newlines: false,
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
        if is_cloud_placeholder(&meta) {
            p.not_editable = Some("not available locally".into());
            p.content = Content::Message(
                "Not fully available on this device. Preview skipped to avoid a cloud download. Download it in your sync app, then select another result and return.".into(),
            );
            return p;
        }
        p.content = if is_dir { load_dir(path, stale) } else { p.load_file(&meta) };
        p
    }

    /// Like [`Preview::load`], but a truncated text file is also read in full for editing, so
    /// that read happens on the preview thread instead of the UI thread.
    pub fn load_for_edit(path: &Path) -> Preview {
        let mut p = Preview::load(path, false);
        if p.needs_full_read() {
            match p.read_full() {
                Ok(text) => p.edit_text = Some(text),
                Err(reason) => p.not_editable = Some(reason),
            }
        }
        p
    }

    /// Whether editing has to wait for [`Preview::load_for_edit`] to read the whole file.
    pub fn needs_full_read(&self) -> bool {
        !self.editing
            && self.can_edit()
            && self.edit_text.is_none()
            && matches!(self.content, Content::Text { truncated: true, .. })
    }

    /// Reads the whole file for editing through one handle, so the bytes and the recorded
    /// version match. The read itself stops past [`EDIT_MAX_BYTES`], even if the file grew
    /// since it was previewed. Errors are reasons for [`Preview::not_editable`].
    fn read_full(&mut self) -> Result<String, String> {
        let file = fs::File::open(&self.path).map_err(|e| format!("file not readable ({e})"))?;
        let meta = file.metadata().map_err(|e| format!("file not readable ({e})"))?;
        if !meta.is_file() {
            return Err("not a regular file".into());
        }
        if is_cloud_placeholder(&meta) {
            return Err("not available locally".into());
        }
        let mut bytes = Vec::new();
        (&file).take(EDIT_MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| format!("file not readable ({e})"))?;
        if bytes.len() as u64 > EDIT_MAX_BYTES {
            return Err(format!("larger than {} MB", EDIT_MAX_BYTES / (1024 * 1024)));
        }
        let version = FileVersion::of(&file, &bytes).map_err(|e| format!("file not readable ({e})"))?;
        let (text, valid) = self.decode(&bytes, false);
        if !valid {
            return Err("not UTF-8 text".into());
        }
        self.loaded = Some(version);
        Ok(text)
    }

    fn load_file(&mut self, meta: &fs::Metadata) -> Content {
        let ext = self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            self.not_editable = Some("image".into());
            return match load_image(&self.path) {
                Ok(pixels) => Content::Image(ImagePreview { pixels: Some(pixels), texture: None }),
                Err(e) => Content::Message(format!("Cannot show image: {e}")),
            };
        }

        // One byte more than is shown tells whether the file is longer, whatever `meta` says now.
        let mut buf = Vec::with_capacity(PREVIEW_BYTES.min(meta.len() as usize) + 1);
        let file = match fs::File::open(&self.path).and_then(|f| (&f).take(PREVIEW_BYTES as u64 + 1).read_to_end(&mut buf).map(|_| f)) {
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

        let truncated = buf.len() > PREVIEW_BYTES;
        buf.truncate(PREVIEW_BYTES);
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
        // Mixed files are saved with their more common ending; the editor says so up front.
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count() - crlf;
        self.crlf = crlf > 0 && crlf >= lf;
        self.mixed_newlines = crlf > 0 && lf > 0;
        (if crlf > 0 { text.replace("\r\n", "\n") } else { text }, valid)
    }

    pub fn can_edit(&self) -> bool {
        matches!(self.content, Content::Text { .. }) && self.not_editable.is_none()
    }

    /// For a file that mixes CRLF and LF, the one line ending every line gets when it is saved.
    pub fn mixed_newlines_saved_as(&self) -> Option<&'static str> {
        self.mixed_newlines.then_some(if self.crlf { "CRLF" } else { "LF" })
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
            // Never read here on the UI thread: `load_for_edit` reads it on the preview thread.
            self.edit_text.take().ok_or("The whole file has not been read for editing yet")?
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

/// Reads and decodes an image within byte, size and memory budgets. Only the first frame of an
/// animation is decoded, and the result is scaled down to [`IMAGE_PREVIEW_SIDE`].
fn load_image(path: &Path) -> Result<egui::ColorImage, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    (&file).take(IMAGE_MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(format!("larger than {} MB", IMAGE_MAX_BYTES / (1024 * 1024)));
    }
    let mut reader = image::ImageReader::new(io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_MAX_SIDE);
    limits.max_image_height = Some(IMAGE_MAX_SIDE);
    limits.max_alloc = Some(IMAGE_MAX_ALLOC);
    reader.limits(limits);
    let mut image = reader.decode().map_err(|e| e.to_string())?;
    if image.width().max(image.height()) > IMAGE_PREVIEW_SIDE {
        image = image.thumbnail(IMAGE_PREVIEW_SIDE, IMAGE_PREVIEW_SIDE);
    }
    let rgba = image.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Offline or recall-on-access files (cloud placeholders) would be downloaded by reading them.
fn is_cloud_placeholder(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS};
        meta.file_attributes() & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
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

/// Lists a folder: folders first, then by case-insensitive name. Only the first [`DIR_LIMIT`]
/// entries in that order are kept (a max-heap of that size), so a folder with millions of
/// entries costs `DIR_LIMIT` entries of memory while the rest are only counted. The listing
/// stops early once `stale` says a newer preview was requested.
fn load_dir(path: &Path, stale: &dyn Fn() -> bool) -> Content {
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return Content::Message(format!("Cannot read folder: {e}")),
    };
    // (not a folder, lowercase name, name): ascending order is display order.
    let mut top: BinaryHeap<(bool, String, String)> = BinaryHeap::with_capacity(DIR_LIMIT + 1);
    let mut total = 0usize;
    for entry in rd.flatten() {
        if total.is_multiple_of(1024) && stale() {
            return Content::Message("Preview cancelled".into());
        }
        total += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = (!entry.file_type().is_ok_and(|t| t.is_dir()), name.to_lowercase(), name);
        if top.len() < DIR_LIMIT {
            top.push(key);
        } else if top.peek().is_some_and(|last| key < *last) {
            top.pop();
            top.push(key);
        }
    }
    let entries = top.into_sorted_vec().into_iter().map(|(not_dir, _, name)| (!not_dir, name)).collect();
    Content::Directory { entries, total }
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

    /// MIN-02: mixed line endings are reported before saving and saved in the more common style.
    #[test]
    fn mixed_line_endings_are_announced() {
        let path = temp("mixed.txt", b"a\r\nb\nc\r\n");
        let mut p = Preview::load(&path, false);
        assert_eq!(p.mixed_newlines_saved_as(), Some("CRLF"));
        p.begin_edit().unwrap();
        assert_eq!(p.buffer, "a\nb\nc\n");
        assert!(p.save(false).is_ok());
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\n");

        // Mostly LF: saved as LF, not converted to CRLF because of a single CRLF.
        let path = temp("mostly-lf.txt", b"a\nb\nc\r\n");
        let mut p = Preview::load(&path, false);
        assert_eq!(p.mixed_newlines_saved_as(), Some("LF"));
        p.begin_edit().unwrap();
        assert!(p.save(false).is_ok());
        assert_eq!(fs::read(&path).unwrap(), b"a\nb\nc\n");

        assert_eq!(Preview::load(&temp("only-crlf.txt", b"a\r\nb\r\n"), false).mixed_newlines_saved_as(), None);
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

    /// FC-05: a file that grew past the limit after it was previewed is not read past the limit,
    /// and a truncated preview is never read in full on the calling (UI) thread.
    #[test]
    fn edit_read_is_capped_and_off_the_ui_thread() {
        let path = temp("grows.txt", &vec![b'a'; 600 * 1024]);
        let mut p = Preview::load(&path, false);
        assert!(p.needs_full_read());
        fs::write(&path, vec![b'b'; 9 * 1024 * 1024]).unwrap();

        assert!(p.begin_edit().is_err());
        assert!(!p.editing && p.buffer.is_empty());
        let reason = p.read_full().unwrap_err();
        assert!(reason.contains("larger than 8 MB"), "{reason}");

        let p = Preview::load_for_edit(&path);
        assert!(!p.can_edit() && !p.needs_full_read());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn large_files_are_read_in_full_for_editing() {
        let mut text = "line\r\n".repeat(100_000);
        let path = temp("large.txt", text.as_bytes());
        let mut p = Preview::load_for_edit(&path);
        assert!(matches!(p.content, Content::Text { truncated: true, .. }));
        assert!(!p.needs_full_read());
        p.begin_edit().unwrap();
        assert_eq!(p.buffer, text.replace("\r\n", "\n"));

        p.buffer.push_str("end\n");
        text.push_str("end\r\n");
        assert!(p.save(false).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_file(path).unwrap();
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

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(&mut io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn binary_and_images_are_not_editable() {
        let bin = temp("data.bin", &[0, 1, 2, 3, 255]);
        let p = Preview::load(&bin, false);
        assert!(matches!(p.content, Content::Binary(_)));
        assert!(!p.can_edit());
        let img = temp("pic.png", &png(2, 2));
        let p = Preview::load(&img, false);
        assert!(matches!(p.content, Content::Image(_)));
        assert!(!p.can_edit());
    }

    /// FC-08: images are read from the local path (no `file://` URI that the loader turned into
    /// a network path), decoded on the preview thread and scaled down for the pane.
    #[test]
    fn images_are_decoded_locally_and_scaled() {
        // `#`, spaces and non-ASCII characters broke URI-based loading.
        let path = temp("wide #1 ünïcode.png", &png(4096, 1024));
        match Preview::load(&path, false).content {
            Content::Image(ImagePreview { pixels: Some(pixels), .. }) => assert_eq!(pixels.size, [2048, 512]),
            _ => panic!("expected a decoded image"),
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn oversized_or_broken_images_are_not_decoded() {
        let broken = temp("broken.png", b"not really a png");
        assert!(matches!(Preview::load(&broken, false).content, Content::Message(m) if m.starts_with("Cannot show image")));
        // A PNG header claiming more pixels than the side limit is refused before decoding.
        let huge = temp("huge.png", &png(IMAGE_MAX_SIDE + 1, 1));
        assert!(matches!(Preview::load(&huge, false).content, Content::Message(m) if m.starts_with("Cannot show image")));
        fs::remove_file(broken).unwrap();
        fs::remove_file(huge).unwrap();
    }

    /// MIN-01: a big folder keeps only `DIR_LIMIT` entries (still in display order) and the
    /// listing stops as soon as a newer preview is requested.
    #[test]
    fn folder_listing_is_bounded_and_cancellable() {
        let dir = std::env::temp_dir().join(format!("filecraft-preview-big-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Zed folder")).unwrap();
        for i in 0..DIR_LIMIT + 5 {
            fs::write(dir.join(format!("file {i:04}.txt")), b"").unwrap();
        }

        let p = Preview::load(&dir, true);
        let Content::Directory { entries, total } = p.content else { panic!("expected a folder listing") };
        assert_eq!(total, DIR_LIMIT + 6);
        assert_eq!(entries.len(), DIR_LIMIT);
        assert_eq!(entries[0], (true, "Zed folder".to_string()), "folders first");
        assert_eq!(entries[1].1, "file 0000.txt");
        assert_eq!(entries[DIR_LIMIT - 1].1, format!("file {:04}.txt", DIR_LIMIT - 2));

        let cancelled = Preview::load_cancellable(&dir, true, &|| true);
        assert!(matches!(cancelled.content, Content::Message(m) if m.contains("cancelled")));
        fs::remove_dir_all(&dir).unwrap();
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
        let image = temp("offline.png", &png(1, 1));
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
            // SAFETY: same NUL-terminated path as above.
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
