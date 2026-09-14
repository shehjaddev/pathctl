//! Relaunch self elevated (UAC) for system-scope writes.

use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Quote an argument per the CommandLineToArgvW rules so the elevated child
/// receives exactly the parent's argv (paths with spaces, quotes or trailing
/// backslashes included). Unquoted plain tokens pass through untouched.
fn quote_arg(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Relaunch the current binary with the `runas` verb, forwarding argv[1..].
///
/// The parent has already passed its confirmation prompt before relaunching,
/// and the elevated child gets no stdin, so `-y` is forced into the child's
/// command line (no re-prompt, no hang).
pub fn relaunch_elevated() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "-y" || a == "--yes") {
        args.push("-y".to_string());
    }
    let cmdline = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let args_wide: Vec<u16> = cmdline
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let res = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_wide.as_ptr(),
            args_wide.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns an HINSTANCE; values <= 32 are SE_ERR_* codes
    // (not GetLastError), so map them to messages directly.
    let code = res as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "elevation launch failed: {}",
            match code {
                0 => "out of memory".to_string(),
                2 => "file not found".to_string(),
                3 => "path not found".to_string(),
                5 => "access denied (UAC declined?)".to_string(),
                8 => "out of memory".to_string(),
                11 => "invalid executable format".to_string(),
                26 => "sharing violation".to_string(),
                27 => "file association incomplete".to_string(),
                28 => "DDE timeout".to_string(),
                29 => "DDE transaction failed".to_string(),
                30 => "DDE busy".to_string(),
                31 => "no application associated".to_string(),
                32 => "DLL not found".to_string(),
                _ => format!("unknown error {code}"),
            }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn plain_args_stay_unquoted() {
        assert_eq!(quote_arg("add"), "add");
        assert_eq!(quote_arg(r"C:\tools"), r"C:\tools");
        assert_eq!(quote_arg("--scope"), "--scope");
    }

    #[test]
    fn spaces_are_quoted() {
        assert_eq!(quote_arg(r"C:\Program Files\X"), r#""C:\Program Files\X""#);
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn trailing_backslash_arg_roundtrips_unquoted() {
        // No spaces/quotes: stays unquoted; a trailing backslash before a
        // space is literal for CommandLineToArgvW.
        assert_eq!(quote_arg(r"C:\"), r"C:\");
        assert_eq!(quote_arg(r"a\b\"), r"a\b\");
    }

    #[test]
    fn trailing_backslashes_are_doubled_inside_quotes() {
        // Space forces quoting; the backslashes before the closing quote must
        // be doubled so the child sees exactly one.
        assert_eq!(quote_arg(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        assert_eq!(quote_arg("a b\\"), r#""a b\\""#);
    }

    #[test]
    fn empty_arg_is_quoted() {
        assert_eq!(quote_arg(""), r#""""#);
    }
}
