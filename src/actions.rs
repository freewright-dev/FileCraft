//! Shell integration: open, reveal, properties, external editor and elevation.

use std::io;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Runs a shell action (open, reveal, properties) on a short-lived thread and hands its result
/// to `done` there. The shell resolves associations, loads handlers and builds item lists for
/// the path, which takes seconds on a slow network share or a cloud placeholder, so these calls
/// must not block the UI thread. The thread enters a single-threaded COM apartment, which shell
/// handlers expect, and [`shell_execute`] waits for the launch to finish before the thread ends.
pub fn in_background(
    action: impl FnOnce() -> io::Result<()> + Send + 'static,
    done: impl FnOnce(io::Result<()>) + Send + 'static,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("shell".into())
        .spawn(move || {
            #[cfg(windows)]
            let _com = ComApartment::enter();
            done(action());
        })
        .map(|_| ())
}

/// COM initialised on the current thread until dropped.
#[cfg(windows)]
struct ComApartment {
    entered: bool,
}

#[cfg(windows)]
impl ComApartment {
    fn enter() -> ComApartment {
        use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
        // SAFETY: plain call; a success (including S_FALSE) is balanced by `CoUninitialize` in drop.
        let hr = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        ComApartment { entered: hr >= 0 }
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.entered {
            // SAFETY: balances the successful `CoInitializeEx` on this same thread.
            unsafe { windows_sys::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// `ShellExecuteExW` with the given verb (`open`, `properties`, …) and extra `SEE_MASK_*` flags.
/// `SEE_MASK_NOASYNC` makes it return only once the launch (DDE included) is done, as required
/// on a thread that ends right after, like the ones [`in_background`] starts. The shell may show
/// its own UI (the "How do you want to open this file?" dialog, error messages).
#[cfg(windows)]
fn shell_execute(verb: &str, file: &Path, mask: u32) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide(verb);
    let file = wide(&file.to_string_lossy());
    // SAFETY: zero is a valid initial state for this plain struct.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC | mask;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: `info` is initialised and its strings outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        return Ok(());
    }
    let last = io::Error::last_os_error();
    shell_failure(last, info.hInstApp as isize).map_or(Ok(()), Err)
}

/// The error to report for a failed `ShellExecuteExW`, given the thread's last error and
/// `hInstApp`. `None` when the last error is `ERROR_CANCELLED`: the shell has already shown its
/// error, or the user cancelled one of its prompts, so there is nothing more to say. Otherwise
/// `hInstApp` holds ShellExecute's own code, which is preferred over the last error (MIN-04);
/// that is the fallback when there is no such code.
#[cfg_attr(not(windows), allow(dead_code))]
fn shell_failure(last: io::Error, inst_app: isize) -> Option<io::Error> {
    const ERROR_CANCELLED: i32 = 1223;
    if last.raw_os_error() == Some(ERROR_CANCELLED) {
        None
    } else if (1..=32).contains(&inst_app) {
        Some(shell_error(inst_app))
    } else {
        Some(last)
    }
}

/// Describes a ShellExecute failure code (0 or an `ERROR_*` / `SE_ERR_*` value up to 32).
#[cfg_attr(not(windows), allow(dead_code))]
fn shell_error(code: isize) -> io::Error {
    let message = match code {
        0 => "out of memory or resources",
        2 => "file not found",
        3 => "path not found",
        5 => "access denied, or the administrator prompt was cancelled",
        8 => "not enough memory",
        11 => "the program is not a valid Windows application",
        26 => "a sharing violation occurred",
        27 => "the file association is incomplete or invalid",
        28..=30 =>"the program did not respond to the request (DDE)",
        31 => "no application is associated with this kind of file",
        32 => "a required DLL was not found",
        _ => return io::Error::other(format!("ShellExecute failed with code {code}")),
    };
    let kind = match code {
        2 | 3 => io::ErrorKind::NotFound,
        5 => io::ErrorKind::PermissionDenied,
        0 | 8 => io::ErrorKind::OutOfMemory,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, message)
}

/// Opens a file or folder with its default application.
pub fn open(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        shell_execute("open", path, 0)
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(path).spawn().map(|_| ())
    }
}

/// Opens Explorer with the item selected.
pub fn reveal(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new(explorer_exe()?).raw_arg(select_arg(path)?).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn().map(|_| ())
    }
}

/// `%SystemRoot%\explorer.exe`. A bare `explorer` is looked up in FileCraft's own folder before
/// System32 and `PATH`, so an `explorer.exe` next to a portable `FileCraft.exe` would run instead.
#[cfg(windows)]
fn explorer_exe() -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the buffer is writable and its length in UTF-16 units is passed.
    let len = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // A longer result is the size the buffer would need, not a path.
    let dir = buf.get(..len).ok_or_else(|| io::Error::other("the Windows folder path is too long"))?;
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(dir)).join("explorer.exe"))
}

/// Explorer's `/select,"…"` argument. Explorer parses its own command line and has no escape for
/// `"`, so a quote in the path would end the argument and let in other switches (FC-17). Win32
/// names cannot contain one, so such paths are refused.
#[cfg_attr(not(windows), allow(dead_code))]
fn select_arg(path: &Path) -> io::Result<String> {
    let path = path.display().to_string();
    if path.contains('"') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the path contains a quote"));
    }
    Ok(format!("/select,\"{path}\""))
}

/// Shows the Windows file properties dialog.
pub fn properties(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        shell_execute("properties", path, windows_sys::Win32::UI::Shell::SEE_MASK_INVOKEIDLIST)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::other("not supported on this platform"))
    }
}

/// The editor command: `FILECRAFT_EDITOR`, `VISUAL`, `EDITOR`, else Notepad.
pub fn default_editor() -> String {
    ["FILECRAFT_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "notepad".to_string())
}

/// The program `editor` starts. It is resolved once: [`edit_with`] runs exactly this file, so
/// the program shown for confirmation is the one that runs.
pub fn editor_program(editor: &str) -> io::Result<std::path::PathBuf> {
    let program = split_command_line(editor).into_iter().next().unwrap_or_else(|| "notepad".to_string());
    #[cfg(windows)]
    {
        resolve_program(&program)
    }
    #[cfg(not(windows))]
    {
        Ok(program.into())
    }
}

/// Launches `program`, as [`editor_program`] resolved it for `editor` (a command line such as
/// `code -n`), with the editor's arguments and the file appended.
pub fn edit_with(program: &Path, editor: &str, path: &Path) -> io::Result<()> {
    editor_command(program, editor, path).spawn().map(|_| ())
}

/// Builds the editor process. The file path is always a separate argument, never shell text:
/// the editor runs directly instead of through `cmd /C`, so names like `a&calc&.txt` stay literal.
fn editor_command(program: &Path, editor: &str, path: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(split_command_line(editor).into_iter().skip(1)).arg(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if has_extension(program, &["bat", "cmd"]) {
            // Shims such as VS Code's `code.cmd`: std runs them via cmd.exe and escapes every
            // argument for it, refusing ones it cannot escape safely. No console window flashes.
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
    cmd
}

/// Splits a command line on whitespace; double quotes group words, so
/// `"C:\Program Files\Editor\editor.exe" -n` keeps its path intact.
fn split_command_line(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut in_word) = (false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    parts.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        parts.push(word);
    }
    parts
}

/// Kinds of file an editor command may name; other PATHEXT entries (`.vbs`, `.js`, …) are refused.
#[cfg(windows)]
const EDITOR_EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];

#[cfg(windows)]
fn has_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension().is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Finds the file `program` names, like cmd would but never relative to the current directory:
/// a full path is taken as given, a bare name is looked up in the absolute `PATH` entries, and
/// PATHEXT extensions are tried. The result is absolute, and it is exactly what gets launched.
#[cfg(windows)]
fn resolve_program(program: &str) -> io::Result<std::path::PathBuf> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    resolve_program_in(program, std::env::var_os("PATH").as_deref(), &pathext)
}

/// [`resolve_program`] with the `PATH` and `PATHEXT` values given.
#[cfg(windows)]
fn resolve_program_in(program: &str, path_var: Option<&std::ffi::OsStr>, pathext: &str) -> io::Result<std::path::PathBuf> {
    use std::path::PathBuf;
    let exts: Vec<&str> = pathext
        .split(';')
        .map(|e| e.trim().trim_start_matches('.'))
        .filter(|e| EDITOR_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .collect();
    let find = |base: PathBuf| -> Option<PathBuf> {
        if has_extension(&base, &EDITOR_EXTENSIONS) && base.is_file() {
            return Some(base);
        }
        exts.iter()
            .map(|e| {
                let mut s = base.clone().into_os_string();
                s.push(".");
                s.push(e);
                PathBuf::from(s)
            })
            .find(|p| p.is_file())
    };
    let found = if program.contains(['\\', '/', ':']) {
        if !Path::new(program).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("editor “{program}” is a relative path; give its full path or a name on PATH"),
            ));
        }
        find(PathBuf::from(program))
    } else {
        // Empty and relative entries (`a;;b`, `.\bin`) would be looked up in the current
        // directory, and the program found there is the one that would run.
        path_var
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| dir.is_absolute())
            .find_map(|dir| find(dir.join(program)))
    };
    found.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("editor “{program}” not found (expected an .exe, .com, .bat or .cmd)"))
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_editor_command_lines() {
        assert_eq!(split_command_line("code -n"), ["code", "-n"]);
        assert_eq!(
            split_command_line(r#""C:\Program Files\Editor\editor.exe"  -n --wait"#),
            [r"C:\Program Files\Editor\editor.exe", "-n", "--wait"]
        );
        assert_eq!(split_command_line(r#"ed "" x"#), ["ed", "", "x"]);
        assert!(split_command_line("   ").is_empty());
    }

    /// MIN-04: ShellExecute's own codes are reported, not an unrelated "last error".
    #[test]
    fn shell_execute_codes_are_described() {
        assert_eq!(shell_error(31).to_string(), "no application is associated with this kind of file");
        assert_eq!(shell_error(2).kind(), io::ErrorKind::NotFound);
        assert_eq!(shell_error(5).kind(), io::ErrorKind::PermissionDenied);
        assert!(shell_error(17).to_string().contains("17"));
    }

    /// Shell actions ran on the UI thread. They now run on a worker, and a failure comes back
    /// from it. (A refused Explorer argument fails before any shell UI could appear.)
    #[cfg(windows)]
    #[test]
    fn shell_actions_report_failures_from_a_worker_thread() {
        let caller = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();
        let quoted = Path::new(r#"C:\dir\a" /root,"C:\Windows"#);
        in_background(move || reveal(quoted), move |result| tx.send((std::thread::current().id(), result)).unwrap()).unwrap();
        let (worker, result) = rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        assert_ne!(worker, caller);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    /// When the shell has shown its own error (or the user cancelled its prompt), it reports
    /// `ERROR_CANCELLED`; that is not reported a second time in the status bar.
    #[test]
    fn errors_the_shell_already_showed_are_not_reported_again() {
        assert!(shell_failure(io::Error::from_raw_os_error(1223), 2).is_none());
        let not_found = shell_failure(io::Error::from_raw_os_error(2), 2).unwrap();
        assert_eq!(not_found.to_string(), "file not found");
        let other = shell_failure(io::Error::from_raw_os_error(1155), 42).unwrap();
        assert_eq!(other.raw_os_error(), Some(1155));
    }

    /// FC-17: a quote in the path used to end Explorer's argument and add switches of its own.
    #[test]
    fn reveal_refuses_quotes_in_paths() {
        assert_eq!(select_arg(Path::new(r"C:\dir\a b.txt")).unwrap(), r#"/select,"C:\dir\a b.txt""#);
        assert!(select_arg(Path::new(r#"C:\dir\a" /root,"C:\Windows"#)).is_err());
    }

    /// Show in folder ran a bare `explorer`, which an `explorer.exe` next to FileCraft overrides.
    #[cfg(windows)]
    #[test]
    fn explorer_is_started_from_the_windows_folder() {
        let exe = explorer_exe().unwrap();
        assert!(exe.is_absolute() && exe.is_file(), "{}", exe.display());
        assert!(exe.file_name().unwrap().eq_ignore_ascii_case("explorer.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn resolves_bare_names_on_path() {
        let cmd = resolve_program("cmd").unwrap();
        assert!(cmd.file_name().unwrap().eq_ignore_ascii_case("cmd.exe"), "{}", cmd.display());
        assert!(resolve_program("filecraft-no-such-editor").is_err());
    }

    /// Empty and relative `PATH` entries, and relative editor paths, used to be resolved against
    /// the current directory, so a program planted there was the one that ran.
    #[cfg(windows)]
    #[test]
    fn never_resolves_against_the_current_directory() {
        use std::ffi::OsString;
        let rel = format!(r"target\filecraft-relative-path-{}", std::process::id());
        let dir = std::env::current_dir().unwrap().join(&rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("planted.cmd"), "@echo off\r\n").unwrap();

        let relative_entries = OsString::from(format!(";{rel};.\\{rel}"));
        let from_relative = resolve_program_in("planted", Some(&relative_entries), ".cmd");
        let explicit_relative = resolve_program_in(&format!(r"{rel}\planted.cmd"), None, ".cmd");
        let absolute_entry = OsString::from(format!(";{}", dir.display()));
        let from_absolute = resolve_program_in("planted", Some(&absolute_entry), ".cmd");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(from_relative.is_err(), "{from_relative:?}");
        assert!(explicit_relative.is_err(), "{explicit_relative:?}");
        assert_eq!(from_absolute.unwrap(), dir.join("planted.cmd"));
    }

    /// FC-01: shell syntax in a file name must reach a `.cmd` editor shim as one literal argument.
    #[cfg(windows)]
    #[test]
    fn file_name_cannot_inject_commands() {
        let dir = std::env::temp_dir().join(format!("filecraft-editor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("shim.cmd");
        std::fs::write(&shim, "@echo off\r\n>\"%~dp0args.txt\" echo(%*\r\n").unwrap();
        let marker = dir.join("injected");
        let name = format!(r"{}\report&type nul>{}&rem %PATH%.txt", dir.display(), marker.display());

        let editor = format!("\"{}\"", shim.display());
        let program = editor_program(&editor).unwrap();
        let status = editor_command(&program, &editor, Path::new(&name)).status().unwrap();
        assert!(status.success());
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let injected = marker.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!injected, "file name ran as a command");
        assert!(args.contains("report&type nul>"), "{args}");
        assert!(args.contains("%PATH%"), "{args}");
    }

    /// FC-18: the elevated confirmation shows the resolved program and the launch used to look
    /// the command up again. The launch now runs the program it is given, whatever the command's
    /// first word would resolve to by then; only the arguments come from the command.
    #[cfg(windows)]
    #[test]
    fn launches_the_program_that_was_resolved() {
        let dir = std::env::temp_dir().join(format!("filecraft-editor-resolved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("shim.cmd");
        std::fs::write(&shim, "@echo off\r\n>\"%~dp0args.txt\" echo(%*\r\n").unwrap();

        let status = editor_command(&shim, "filecraft-no-such-editor -n", Path::new("file.txt")).status().unwrap();
        let args = std::fs::read_to_string(dir.join("args.txt"));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(status.success());
        assert_eq!(args.unwrap().trim(), "-n file.txt");
    }
}
