//! Shell integration: open, reveal, properties, external editor and elevation.

use std::io;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// `ShellExecuteW` with the given verb (`open`, `runas`, …).
#[cfg(windows)]
fn shell_execute(verb: &str, file: &str, params: Option<&str>) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide(verb), wide(file));
    let params = params.map(wide);
    // SAFETY: all strings are NUL-terminated UTF-16 and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if result as isize > 32 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Opens a file or folder with its default application.
pub fn open(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        shell_execute("open", &path.to_string_lossy(), None)
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
        Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn().map(|_| ())
    }
}

/// Shows the Windows file properties dialog.
pub fn properties(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let verb = wide("properties");
        let file = wide(&path.to_string_lossy());
        // SAFETY: zero is a valid initial state for this plain struct.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_INVOKEIDLIST;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        // SAFETY: `info` is initialised and its strings outlive the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
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

/// Launches `editor` (a command line such as `code -n`) with the file appended.
pub fn edit_externally(editor: &str, path: &Path) -> io::Result<()> {
    editor_command(editor, path)?.spawn().map(|_| ())
}

/// Builds the editor process. The file path is always a separate argument, never shell text:
/// the editor runs directly instead of through `cmd /C`, so names like `a&calc&.txt` stay literal.
fn editor_command(editor: &str, path: &Path) -> io::Result<Command> {
    let mut parts = split_command_line(editor).into_iter();
    let program = parts.next().unwrap_or_else(|| "notepad".to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let program = resolve_program(&program)?;
        let mut cmd = Command::new(&program);
        cmd.args(parts).arg(path);
        if has_extension(&program, &["bat", "cmd"]) {
            // Shims such as VS Code's `code.cmd`: std runs them via cmd.exe and escapes every
            // argument for it, refusing ones it cannot escape safely. No console window flashes.
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        Ok(cmd)
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(program);
        cmd.args(parts).arg(path);
        Ok(cmd)
    }
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

/// Finds the file `program` names, like cmd would but without searching the current directory:
/// a path is taken as given, a bare name is looked up on PATH, and PATHEXT extensions are tried.
#[cfg(windows)]
fn resolve_program(program: &str) -> io::Result<std::path::PathBuf> {
    use std::path::PathBuf;
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
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
        find(PathBuf::from(program))
    } else {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .find_map(|dir| find(dir.join(program)))
    };
    found.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("editor “{program}” not found (expected an .exe, .com, .bat or .cmd)"))
    })
}

/// Starts a new elevated instance of this program (UAC prompt). Returns Ok when it launched.
pub fn restart_elevated() -> io::Result<()> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()?;
        shell_execute("runas", &exe.to_string_lossy(), None)
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::other("not supported on this platform"))
    }
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

    #[cfg(windows)]
    #[test]
    fn resolves_bare_names_on_path() {
        let cmd = resolve_program("cmd").unwrap();
        assert!(cmd.file_name().unwrap().eq_ignore_ascii_case("cmd.exe"), "{}", cmd.display());
        assert!(resolve_program("filecraft-no-such-editor").is_err());
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

        let status = editor_command(&format!("\"{}\"", shim.display()), Path::new(&name))
            .unwrap()
            .status()
            .unwrap();
        assert!(status.success());
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let injected = marker.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!injected, "file name ran as a command");
        assert!(args.contains("report&type nul>"), "{args}");
        assert!(args.contains("%PATH%"), "{args}");
    }
}
