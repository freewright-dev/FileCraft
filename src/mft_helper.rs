//! Privileged NTFS reading in a separate process (FC-18).
//!
//! Reading the master file table needs administrator rights, but the window should not have
//! them: elevated, every action it takes (opening files, running the editor, writing caches and
//! backups) would run as administrator on state that unelevated programs of the same user can
//! change. So the window stays unelevated, and "fast NTFS indexing" starts this small helper
//! elevated instead (`FileCraft.exe --mft-helper <pipe>`, one UAC prompt).
//!
//! The helper only reads volumes. It connects to a named pipe the window created, answers
//! "index drive X" requests with progress and the index in the cache format, and exits when the
//! pipe closes. It writes no files, reads no settings and starts no programs. The window checks
//! everything it receives exactly like a cache file ([`VolumeIndex::from_bytes`]).
//!
//! Protocol, little-endian: the window sends `R` and a drive letter; the helper sends any number
//! of `P` + progress (u64) + total (u64), then `D` + length (u64) + index, or `E` + length (u32)
//! + UTF-8 message.

use crate::index::VolumeIndex;
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Command-line switch that runs the helper instead of the window.
pub const HELPER_ARG: &str = "--mft-helper";
const PIPE_PREFIX: &str = r"\\.\pipe\filecraft-mft-";
const MAX_ERROR_BYTES: u32 = 64 * 1024;
/// Same bound as a cache file.
const MAX_INDEX_BYTES: u64 = 4 << 30;
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

type ReadVolume<'a> = dyn Fn(char, &AtomicUsize, &AtomicUsize, &AtomicBool) -> anyhow::Result<VolumeIndex> + Sync + 'a;

// ─── Protocol ───────────────────────────────────────────────────────────────

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// The next requested drive letter, or `None` once the window has closed the pipe.
fn read_request(r: &mut impl Read) -> io::Result<Option<char>> {
    let mut req = [0u8; 2];
    match r.read_exact(&mut req) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
        Err(e) => return Err(e),
    }
    match req {
        [b'R', letter @ b'A'..=b'Z'] => Ok(Some(letter as char)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad request")),
    }
}

/// The helper's side: answers requests until the window closes the pipe.
fn serve(conn: &mut (impl Read + Write), read_volume: &ReadVolume) -> io::Result<()> {
    while let Some(letter) = read_request(conn)? {
        let (progress, total, cancel) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicBool::new(false));
        let result = std::thread::scope(|s| -> io::Result<_> {
            let job = s.spawn(|| read_volume(letter, &progress, &total, &cancel));
            while !job.is_finished() {
                if let Err(e) = send_progress(conn, &progress, &total) {
                    // The window is gone: stop reading the volume rather than finish for nobody.
                    cancel.store(true, Ordering::Relaxed);
                    return Err(e);
                }
                std::thread::sleep(PROGRESS_EVERY);
            }
            Ok(job.join())
        })?;
        match result {
            Ok(Ok(v)) => {
                let mut bytes = Vec::new();
                v.serialize(&mut bytes)?;
                conn.write_all(b"D")?;
                conn.write_all(&(bytes.len() as u64).to_le_bytes())?;
                conn.write_all(&bytes)?;
            }
            Ok(Err(e)) => send_error(conn, &e.to_string())?,
            Err(_) => send_error(conn, "the NTFS reader panicked")?,
        }
        conn.flush()?;
    }
    Ok(())
}

fn send_progress(conn: &mut impl Write, progress: &AtomicUsize, total: &AtomicUsize) -> io::Result<()> {
    conn.write_all(b"P")?;
    conn.write_all(&(progress.load(Ordering::Relaxed) as u64).to_le_bytes())?;
    conn.write_all(&(total.load(Ordering::Relaxed) as u64).to_le_bytes())?;
    conn.flush()
}

fn send_error(conn: &mut impl Write, message: &str) -> io::Result<()> {
    let bytes = &message.as_bytes()[..message.len().min(MAX_ERROR_BYTES as usize)];
    conn.write_all(b"E")?;
    conn.write_all(&(bytes.len() as u32).to_le_bytes())?;
    conn.write_all(bytes)
}

/// What the helper answered to one request. Both leave the pipe in step for the next request.
enum Reply {
    /// The serialized index, not yet checked.
    Index(Vec<u8>),
    /// The helper's reader failed with this message.
    Failed(String),
}

/// The window's side of one request. Any error means the pipe is out of step (or broken).
fn request(conn: &mut (impl Read + Write), letter: char, progress: &AtomicUsize, total: &AtomicUsize) -> anyhow::Result<Reply> {
    conn.write_all(&[b'R', letter as u8])?;
    conn.flush()?;
    loop {
        let mut kind = [0u8; 1];
        conn.read_exact(&mut kind)?;
        match kind[0] {
            b'P' => {
                progress.store(read_u64(conn)? as usize, Ordering::Relaxed);
                total.store(read_u64(conn)? as usize, Ordering::Relaxed);
            }
            b'D' => {
                let len = read_u64(conn)?;
                anyhow::ensure!(len <= MAX_INDEX_BYTES, "the NTFS helper sent an index over 4 GiB");
                let mut data = Vec::new();
                conn.take(len).read_to_end(&mut data)?;
                anyhow::ensure!(data.len() as u64 == len, "the NTFS helper's index was cut short");
                return Ok(Reply::Index(data));
            }
            b'E' => {
                let mut len = [0u8; 4];
                conn.read_exact(&mut len)?;
                let len = u32::from_le_bytes(len);
                anyhow::ensure!(len <= MAX_ERROR_BYTES, "bad message from the NTFS helper");
                let mut msg = vec![0u8; len as usize];
                conn.read_exact(&mut msg)?;
                return Ok(Reply::Failed(String::from_utf8_lossy(&msg).into_owned()));
            }
            _ => anyhow::bail!("bad message from the NTFS helper"),
        }
    }
}

// ─── The window's side ──────────────────────────────────────────────────────

/// Longest silence from the helper during a request. While it reads it sends progress every
/// [`PROGRESS_EVERY`], so this passes only when it hangs (a stuck disk, or a stuck helper).
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The window's end of the pipe during one request: notes when the helper last sent something
/// and fails every read once the watchdog has given up on it.
struct Watched<'a> {
    pipe: &'a mut std::fs::File,
    start: Instant,
    /// Milliseconds after `start` when bytes last arrived.
    heard: &'a AtomicU64,
    timed_out: &'a AtomicBool,
}

impl Read for Watched<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.timed_out.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = self.pipe.read(buf)?;
        self.heard.store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Write for Watched<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pipe.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

/// Runs until `done` closes. Once nothing has been `heard` for `idle`, marks the request as
/// `timed_out` and keeps calling `interrupt`: a read can start just after an interruption.
fn watchdog(done: std::sync::mpsc::Receiver<()>, idle: Duration, start: Instant, heard: &AtomicU64, timed_out: &AtomicBool, interrupt: impl Fn()) {
    let poll = (idle / 4).min(Duration::from_secs(1));
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done.recv_timeout(poll) {
        let silent = start.elapsed().saturating_sub(Duration::from_millis(heard.load(Ordering::Relaxed)));
        if silent > idle || timed_out.load(Ordering::Relaxed) {
            timed_out.store(true, Ordering::Relaxed);
            interrupt();
        }
    }
}

/// A function that aborts the I/O blocked on `pipe`, from any thread.
#[cfg(windows)]
fn interrupter(pipe: &std::fs::File) -> impl Fn() + Send + use<> {
    use std::os::windows::io::AsRawHandle;
    let handle = pipe.as_raw_handle() as usize;
    move || {
        // SAFETY: only called while `read_volume` holds the pipe open; a handle without
        // pending I/O is left as it is.
        unsafe { windows_sys::Win32::System::IO::CancelIoEx(handle as _, std::ptr::null()) };
    }
}

#[cfg(not(windows))]
fn interrupter(_: &std::fs::File) -> impl Fn() + Send + use<> {
    || {}
}

/// A running helper, reached through its pipe. Requests are served one at a time.
pub struct MftHelper {
    conn: Mutex<Option<std::fs::File>>,
    /// Cleared when the pipe is dropped. Kept outside `conn`, whose lock is held for a whole
    /// request, so the UI can ask without waiting for a drive to be read.
    alive: AtomicBool,
    idle_timeout: Duration,
}

impl MftHelper {
    fn new(pipe: std::fs::File) -> MftHelper {
        MftHelper { conn: Mutex::new(Some(pipe)), alive: AtomicBool::new(true), idle_timeout: IDLE_TIMEOUT }
    }

    /// Whether the helper still answers (it is dropped after the first broken exchange).
    /// Never blocks: the status bar asks every frame.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Reads drive `letter`'s master file table in the helper. Requests wait for each other;
    /// one whose job was `cancel`led while it waited is not sent. A helper that stays silent
    /// for [`IDLE_TIMEOUT`] is given up on.
    pub fn read_volume(&self, letter: char, progress: &AtomicUsize, total: &AtomicUsize, cancel: &AtomicBool) -> anyhow::Result<VolumeIndex> {
        anyhow::ensure!(letter.is_ascii_uppercase(), "bad drive letter");
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
        let Some(pipe) = conn.as_mut() else { anyhow::bail!("the NTFS helper has stopped") };
        let (start, heard, timed_out) = (Instant::now(), AtomicU64::new(0), AtomicBool::new(false));
        let interrupt = interrupter(pipe);
        let reply = std::thread::scope(|s| {
            let (done, done_rx) = std::sync::mpsc::channel::<()>();
            let (heard, timed_out, idle) = (&heard, &timed_out, self.idle_timeout);
            s.spawn(move || watchdog(done_rx, idle, start, heard, timed_out, interrupt));
            let reply = request(&mut Watched { pipe, start, heard, timed_out }, letter, progress, total);
            drop(done);
            reply
        });
        // Only a failed exchange leaves the pipe out of step. A reader error or an index that
        // fails the checks below arrived whole, so the helper stays usable.
        match reply {
            Ok(Reply::Index(data)) => Ok(VolumeIndex::from_bytes(&data, letter)?),
            Ok(Reply::Failed(message)) => anyhow::bail!("{message}"),
            Err(e) => {
                *conn = None;
                self.alive.store(false, Ordering::Relaxed);
                if timed_out.load(Ordering::Relaxed) {
                    anyhow::bail!("the NTFS helper stopped answering");
                }
                Err(e)
            }
        }
    }

    /// Starts the helper elevated (UAC prompt) and waits until it has connected. Blocks, so
    /// call it off the UI thread.
    #[cfg(windows)]
    pub fn launch() -> io::Result<MftHelper> {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let name = format!("{PIPE_PREFIX}{}-{:016x}", std::process::id(), hasher.finish());

        let pipe = win::create_server(&name)?;
        let process = win::start_elevated(&format!("{HELPER_ARG} {name}"))?;
        win::wait_connected(&pipe, win::process_id(&process)?, || win::has_exited(&process), Duration::from_secs(60))?;
        Ok(MftHelper::new(std::fs::File::from(pipe)))
    }

    #[cfg(not(windows))]
    pub fn launch() -> io::Result<MftHelper> {
        Err(io::Error::other("NTFS indexing is only available on Windows"))
    }
}

// ─── The helper's side ──────────────────────────────────────────────────────

/// Entry point of `FileCraft.exe --mft-helper <pipe>`; returns the process exit code.
pub fn run_helper(pipe: &std::ffi::OsStr) -> i32 {
    let Some(name) = pipe.to_str().filter(|n| valid_pipe_name(n)) else { return 2 };
    let Ok(mut conn) = open_client(name) else { return 3 };
    match serve(&mut conn, &crate::mft::read_volume) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Only pipes FileCraft creates: `\\.\pipe\filecraft-mft-<pid>-<hex>`.
fn valid_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.len() <= 40 && rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

/// Opens the window's pipe. With `SECURITY_IDENTIFICATION`, the pipe's server (or a program that
/// took the name first) can learn who connected but cannot act with the helper's elevated token.
/// While another program holds the pipe, waits for the window to turn it away.
fn open_client(name: &str) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.security_qos_flags(windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION);
    }
    let start = Instant::now();
    loop {
        match options.open(name) {
            #[cfg(windows)]
            Err(e) if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) && start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, SetNamedPipeHandleState,
    };
    use windows_sys::Win32::System::Threading::{GetProcessId, WaitForSingleObject};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A single-instance, local-only pipe. `FILE_FLAG_FIRST_PIPE_INSTANCE` fails if another
    /// program already created one with this name, so the helper can only reach this window.
    pub fn create_server(name: &str) -> io::Result<OwnedHandle> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call; default security.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1 << 16,
                1 << 16,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a valid pipe handle that nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// Waits until process `client_pid` has connected, then switches the pipe to blocking I/O.
    /// Anyone can open the pipe by name, so any other client is disconnected and the wait goes
    /// on: only the helper this window started may talk to it.
    pub fn wait_connected(pipe: &OwnedHandle, client_pid: u32, gave_up: impl Fn() -> bool, timeout: Duration) -> io::Result<()> {
        let start = Instant::now();
        loop {
            // SAFETY: a live pipe handle in non-blocking mode; no OVERLAPPED.
            let ok = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) };
            // SAFETY: no arguments.
            let err = unsafe { GetLastError() };
            // A nonzero result means the pipe is listening again after a disconnect; only
            // ERROR_PIPE_CONNECTED reports a connected client.
            if ok == 0 && err == ERROR_PIPE_CONNECTED {
                if client_process_id(pipe) == Some(client_pid) {
                    break;
                }
                disconnect(pipe)?;
            } else if ok == 0 && err == ERROR_NO_DATA {
                // A client connected and closed its end before it was checked.
                disconnect(pipe)?;
            } else if ok == 0 && err != ERROR_PIPE_LISTENING {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            if gave_up() {
                return Err(io::Error::other("the NTFS helper stopped before connecting"));
            }
            if start.elapsed() > timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the NTFS helper did not connect"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
        // SAFETY: a live pipe handle; `mode` is readable for the call, the other fields stay as they are.
        if unsafe { SetNamedPipeHandleState(pipe.as_raw_handle(), &mode, std::ptr::null(), std::ptr::null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn client_process_id(pipe: &OwnedHandle) -> Option<u32> {
        let mut pid = 0u32;
        // SAFETY: a live, connected pipe handle and a writable u32.
        (unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } != 0).then_some(pid)
    }

    fn disconnect(pipe: &OwnedHandle) -> io::Result<()> {
        // SAFETY: a live pipe handle; the client's end is closed by force.
        if unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn process_id(process: &OwnedHandle) -> io::Result<u32> {
        // SAFETY: a live process handle.
        match unsafe { GetProcessId(process.as_raw_handle()) } {
            0 => Err(io::Error::last_os_error()),
            pid => Ok(pid),
        }
    }

    /// Starts this program elevated with `params` and returns its process handle.
    pub fn start_elevated(params: &str) -> io::Result<OwnedHandle> {
        use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let exe = wide(&std::env::current_exe()?.to_string_lossy());
        let (verb, params) = (wide("runas"), wide(params));
        // SAFETY: zero is a valid initial state for this plain struct.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        // SAFETY: `info` is initialised and its strings outlive the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.hProcess.is_null() {
            return Err(io::Error::other("the NTFS helper did not start"));
        }
        // SAFETY: SEE_MASK_NOCLOSEPROCESS hands us the process handle to close.
        Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess) })
    }

    pub fn has_exited(process: &OwnedHandle) -> bool {
        // SAFETY: a live process handle; zero timeout.
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    /// A job that was never cancelled.
    static GO: AtomicBool = AtomicBool::new(false);

    fn pipe_name(label: &str) -> String {
        format!("{PIPE_PREFIX}{}-{label}", std::process::id())
    }

    /// Starts `serve` with `read` on a fresh pipe and connects the window's side to it.
    fn connect(
        label: &str,
        read: impl Fn(char, &AtomicUsize, &AtomicUsize) -> anyhow::Result<VolumeIndex> + Send + Sync + 'static,
    ) -> (MftHelper, std::thread::JoinHandle<io::Result<()>>) {
        let name = pipe_name(label);
        let server = win::create_server(&name).unwrap();
        let read = move |letter, progress: &AtomicUsize, total: &AtomicUsize, _: &AtomicBool| read(letter, progress, total);
        let helper = std::thread::spawn(move || serve(&mut open_client(&name).unwrap(), &read));
        win::wait_connected(&server, std::process::id(), || false, Duration::from_secs(10)).unwrap();
        (MftHelper::new(std::fs::File::from(server)), helper)
    }

    /// The window and helper sides end to end over a real pipe (without elevation): progress,
    /// an index that passes the cache checks, a reader error, and the helper ending when the
    /// window closes the pipe.
    #[test]
    fn window_and_helper_talk_over_a_pipe() {
        let (window, helper) = connect("a1", |letter, progress, total| {
            total.store(10, Ordering::Relaxed);
            progress.store(10, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(200));
            anyhow::ensure!(letter == 'C', "no NTFS volume {letter}:");
            Ok(sample())
        });

        let (progress, total) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let v = window.read_volume('C', &progress, &total, &GO).unwrap();
        assert_eq!(v.len(), sample().len());
        assert_eq!(total.load(Ordering::Relaxed), 10);

        let err = window.read_volume('D', &progress, &total, &GO).err().expect("drive D: must fail");
        assert!(err.to_string().contains("no NTFS volume D:"), "{err}");
        assert!(window.is_alive(), "a reader error keeps the helper");

        drop(window);
        helper.join().unwrap().unwrap();
    }

    /// An index that fails the cache checks arrived whole, so the helper stays in use. It was
    /// dropped before (the check fails with an `io::Error`), which turned fast indexing off and
    /// needed another UAC prompt.
    #[test]
    fn a_rejected_index_keeps_the_helper() {
        // Always drive C:'s index, so a request for D: gets an index for another drive.
        let (window, helper) = connect("d4", |_, _, _| Ok(sample()));
        let (progress, total) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let err = window.read_volume('D', &progress, &total, &GO).err().expect("an index for C: must be rejected");
        assert!(err.to_string().contains("another drive"), "{err}");
        assert!(window.is_alive(), "a rejected index keeps the helper");
        assert_eq!(window.read_volume('C', &progress, &total, &GO).unwrap().len(), sample().len());

        drop(window);
        helper.join().unwrap().unwrap();
    }

    /// A job cancelled while it waited for the pipe does not ask the helper to read the drive.
    /// The flag used to be checked only after the whole read had been sent back.
    #[test]
    fn cancelled_requests_are_not_sent() {
        let reads = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&reads);
        let (window, helper) = connect("e5", move |_, _, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(sample())
        });
        let (progress, total) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let err = window.read_volume('C', &progress, &total, &AtomicBool::new(true)).err().expect("a cancelled job must fail");
        assert_eq!(err.to_string(), "cancelled");
        assert!(window.is_alive());
        assert_eq!(window.read_volume('C', &progress, &total, &GO).unwrap().len(), sample().len());
        assert_eq!(reads.load(Ordering::Relaxed), 1, "only the second request reached the helper");

        drop(window);
        helper.join().unwrap().unwrap();
    }

    /// `is_alive` answers while a request holds the pipe. It used to wait for the pipe's lock,
    /// so the status bar froze the window until every drive had been read.
    #[test]
    fn liveness_does_not_wait_for_a_request() {
        let release = std::sync::Arc::new(AtomicBool::new(false));
        let reader_release = std::sync::Arc::clone(&release);
        let (window, helper) = connect("c3", move |_, _, total| {
            total.store(1, Ordering::Relaxed);
            while !reader_release.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(sample())
        });
        let window = std::sync::Arc::new(window);
        let total = std::sync::Arc::new(AtomicUsize::new(0));
        let request = {
            let (window, total) = (std::sync::Arc::clone(&window), std::sync::Arc::clone(&total));
            std::thread::spawn(move || window.read_volume('C', &AtomicUsize::new(0), &total, &GO).map(|v| v.len()))
        };
        // The first progress message: the request is under way and holds the pipe.
        while total.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = std::sync::Arc::clone(&window);
        let probe = std::thread::spawn(move || tx.send(probe.is_alive()));
        let answer = rx.recv_timeout(Duration::from_secs(2));
        release.store(true, Ordering::Relaxed);
        assert_eq!(answer, Ok(true), "is_alive blocked behind the running request");

        assert_eq!(request.join().unwrap().unwrap(), sample().len());
        probe.join().unwrap().unwrap();
        drop(window);
        helper.join().unwrap().unwrap();
    }

    /// A request whose window has gone: the request arrived, then every write fails.
    struct GoneWindow(io::Cursor<Vec<u8>>);

    impl Read for GoneWindow {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for GoneWindow {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    /// When the window stops listening, the helper cancels the read instead of scanning the
    /// volume to the end for nobody. The reader was given a flag that could never be set.
    #[test]
    fn the_helper_stops_reading_when_the_window_is_gone() {
        let read = |_: char, _: &AtomicUsize, _: &AtomicUsize, cancel: &AtomicBool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !cancel.load(Ordering::Relaxed) {
                anyhow::ensure!(std::time::Instant::now() < deadline, "never cancelled");
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::bail!("cancelled")
        };
        let start = std::time::Instant::now();
        let err = serve(&mut GoneWindow(io::Cursor::new(b"RC".to_vec())), &read).expect_err("the window is gone");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(start.elapsed() < Duration::from_secs(5), "the reader ran on for {:?}", start.elapsed());
    }

    /// A helper that takes a request and then says nothing is given up on after the idle
    /// timeout. The window used to wait on the pipe forever, and with it every later drive.
    #[test]
    fn a_silent_helper_times_out() {
        let name = pipe_name("f6");
        let server = win::create_server(&name).unwrap();
        let release = std::sync::Arc::new(AtomicBool::new(false));
        let hung = std::sync::Arc::clone(&release);
        let helper = std::thread::spawn(move || {
            let mut conn = open_client(&name).unwrap();
            assert_eq!(read_request(&mut conn).unwrap(), Some('C'));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !hung.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        win::wait_connected(&server, std::process::id(), || false, Duration::from_secs(10)).unwrap();
        let mut window = MftHelper::new(std::fs::File::from(server));
        window.idle_timeout = Duration::from_millis(300);

        let start = Instant::now();
        let err = window.read_volume('C', &AtomicUsize::new(0), &AtomicUsize::new(0), &GO).err();
        let waited = start.elapsed();
        release.store(true, Ordering::Relaxed);
        helper.join().unwrap();
        assert!(waited < Duration::from_secs(5), "waited {waited:?} for a silent helper");
        assert_eq!(err.map(|e| e.to_string()).as_deref(), Some("the NTFS helper stopped answering"));
        assert!(!window.is_alive());
    }

    /// Any program of the user can open the pipe by name. The window only accepts the helper it
    /// started: another client is disconnected and the wait goes on, so the real helper can
    /// still connect. The first client used to be trusted, and could feed in a forged index.
    #[test]
    fn only_the_started_helper_may_connect() {
        let name = pipe_name("a7");
        let server = win::create_server(&name).unwrap();
        // This test process connects, while the window waits for some other process.
        let mut impostor = open_client(&name).unwrap();
        let err = win::wait_connected(&server, std::process::id().wrapping_add(1), || false, Duration::from_millis(500)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(impostor.read(&mut [0u8; 1]).is_err(), "the impostor must be disconnected");
        drop(impostor);

        let client = std::thread::spawn(move || open_client(&name));
        win::wait_connected(&server, std::process::id(), || false, Duration::from_secs(10)).unwrap();
        client.join().unwrap().unwrap();
    }

    /// A second server under the same name is refused, so no other program can stand in for
    /// the window; and the helper only accepts FileCraft's own pipe names.
    #[test]
    fn pipes_cannot_be_hijacked() {
        let name = pipe_name("b2");
        let _first = win::create_server(&name).unwrap();
        assert!(win::create_server(&name).is_err());
        assert!(valid_pipe_name(&name));
        assert!(!valid_pipe_name(r"\\.\pipe\other"));
        assert!(!valid_pipe_name(&format!(r"{PIPE_PREFIX}..\x")));
        assert!(!valid_pipe_name(r"C:\Windows\System32\config"));
    }
}
