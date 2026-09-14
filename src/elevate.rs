//! Relaunch self elevated (UAC) for system-scope writes.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Quote an argument per the CommandLineToArgvW rules so the elevated child
/// receives exactly the parent's argv (paths with spaces, quotes or trailing
/// backslashes included). Plain tokens pass through untouched.
///
/// Works on the wide form rather than `&str`: Windows argv is UTF-16 and can
/// hold unpaired surrogates, which `std::env::args()` would panic on.
fn quote_arg(s: &OsStr) -> Vec<u16> {
    let units: Vec<u16> = s.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|u| *u == u16::from(b' ') || *u == u16::from(b'\t') || *u == u16::from(b'"'));
    if !needs_quotes {
        return units;
    }
    let mut out = Vec::with_capacity(units.len() + 2);
    out.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for u in units {
        match u {
            u if u == u16::from(b'\\') => backslashes += 1,
            u if u == u16::from(b'"') => {
                out.extend(std::iter::repeat_n(
                    u16::from(b'\\'),
                    backslashes * 2 + 1,
                ));
                out.push(u16::from(b'"'));
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
                out.push(u);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    out.push(u16::from(b'"'));
    out
}

/// Build a child command line from arguments (NUL-terminated).
fn command_line<S: AsRef<OsStr>>(args: &[S]) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(u16::from(b' '));
        }
        out.extend(quote_arg(arg.as_ref()));
    }
    out.push(0);
    out
}

/// Relaunch the current binary with the `runas` verb, forwarding argv[1..].
///
/// The parent has already passed its confirmation prompt before relaunching,
/// and the elevated child gets no stdin, so `-y` is forced into the child's
/// command line (no re-prompt, no hang).
pub fn relaunch_elevated() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if !args
        .iter()
        .any(|a| a == OsStr::new("-y") || a == OsStr::new("--yes"))
    {
        args.push(OsString::from("-y"));
    }
    let args_wide = command_line(&args);
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
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
    use super::{command_line, quote_arg};
    use std::ffi::{OsStr, OsString};

    /// Expected quoting result, as a wide string.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn plain_args_stay_unquoted() {
        assert_eq!(quote_arg(OsStr::new("add")), wide("add"));
        assert_eq!(quote_arg(OsStr::new(r"C:\tools")), wide(r"C:\tools"));
        assert_eq!(quote_arg(OsStr::new("--scope")), wide("--scope"));
    }

    #[test]
    fn spaces_are_quoted() {
        assert_eq!(
            quote_arg(OsStr::new(r"C:\Program Files\X")),
            wide(r#""C:\Program Files\X""#)
        );
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(OsStr::new(r#"a"b"#)), wide(r#""a\"b""#));
        assert_eq!(quote_arg(OsStr::new(r#"say "hi""#)), wide(r#""say \"hi\"""#));
    }

    #[test]
    fn trailing_backslash_arg_roundtrips_unquoted() {
        // No spaces/quotes: stays unquoted; a trailing backslash before a
        // space is literal for CommandLineToArgvW.
        assert_eq!(quote_arg(OsStr::new(r"C:\")), wide(r"C:\"));
        assert_eq!(quote_arg(OsStr::new(r"a\b\")), wide(r"a\b\"));
    }

    #[test]
    fn trailing_backslashes_are_doubled_inside_quotes() {
        // Space forces quoting; the backslashes before the closing quote must
        // be doubled so the child sees exactly one.
        assert_eq!(
            quote_arg(OsStr::new(r"C:\Program Files\")),
            wide(r#""C:\Program Files\\""#)
        );
        assert_eq!(quote_arg(OsStr::new("a b\\")), wide(r#""a b\\""#));
    }

    #[test]
    fn empty_arg_is_quoted() {
        assert_eq!(quote_arg(OsStr::new("")), wide(r#""""#));
    }

    #[test]
    fn unpaired_surrogates_survive_quoting() {
        use std::os::windows::ffi::OsStringExt;
        // Windows argv is UTF-16 and may hold unpaired surrogates; forwarding
        // them must neither panic nor mangle the argument.
        let plain = OsString::from_wide(&[0xD800, 0x0041]);
        assert_eq!(quote_arg(&plain), vec![0xD800, 0x0041]);
        let spaced = OsString::from_wide(&[0x0041, 0x0020, 0xD800]);
        assert_eq!(quote_arg(&spaced), vec![0x22, 0x0041, 0x0020, 0xD800, 0x22]);
    }

    #[test]
    fn command_line_joins_with_spaces_and_terminates() {
        assert_eq!(
            command_line(&["add", r"C:\Program Files\x"]),
            wide("add \"C:\\Program Files\\x\"\0")
        );
        assert_eq!(command_line::<&str>(&[]), vec![0]);
    }
}
