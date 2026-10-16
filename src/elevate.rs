//! Relaunch self elevated (UAC) for system-scope writes.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// The ASCII delimiters that matter for quoting, as UTF-16 units.
const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;

/// Quote an argument per the CommandLineToArgvW rules so the elevated child
/// receives exactly the parent's argv (paths with spaces, quotes or trailing
/// backslashes included). Plain tokens pass through untouched.
///
/// Works on the wide form rather than `&str`: Windows argv is UTF-16 and can
/// hold unpaired surrogates, which `std::env::args()` would panic on.
fn quote_arg(s: &OsStr) -> Vec<u16> {
    let units: Vec<u16> = s.encode_wide().collect();
    let needs_quotes = units.is_empty() || units.iter().any(|u| matches!(*u, SPACE | TAB | QUOTE));
    if !needs_quotes {
        return units;
    }
    let mut out = Vec::with_capacity(units.len() + 2);
    out.push(QUOTE);
    let mut backslashes = 0usize;
    for unit in units {
        match unit {
            BACKSLASH => backslashes += 1,
            QUOTE => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
                out.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
                out.push(unit);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    out.push(QUOTE);
    out
}

/// Build a child command line from arguments (NUL-terminated).
fn command_line<S: AsRef<OsStr>>(args: &[S]) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(SPACE);
        }
        out.extend(quote_arg(arg.as_ref()));
    }
    out.push(0);
    out
}

/// The argv handed to the elevated child: the parent's own arguments minus the
/// elevation flag, so the elevated process never spawns another copy, plus `-y`
/// when missing (the child has no stdin, so it must not prompt).
fn child_args<I: IntoIterator<Item = OsString>>(args: I) -> Vec<OsString> {
    let mut out: Vec<OsString> = args
        .into_iter()
        .filter(|a| a != OsStr::new("--elevate") && a != OsStr::new("-e"))
        .collect();
    if !out
        .iter()
        .any(|a| a == OsStr::new("-y") || a == OsStr::new("--yes"))
    {
        out.push(OsString::from("-y"));
    }
    out
}

/// Relaunch the current binary with the `runas` verb, forwarding the child
/// argv, wait for it to finish, and return its exit code so the parent exits
/// with the same status. `ShellExecuteW` returns as soon as the child starts,
/// which would let the parent report success before the write actually ran.
pub fn relaunch_elevated() -> io::Result<u8> {
    let exe = std::env::current_exe()?;
    let args = child_args(std::env::args_os().skip(1));
    let args_wide = command_line(&args);
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.as_ptr(),
        lpFile: exe_wide.as_ptr(),
        lpParameters: args_wide.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        return Err(launch_error(unsafe { GetLastError() }));
    }
    // SEE_MASK_NOCLOSEPROCESS means the child's process handle is ours to close.
    let handle = info.hProcess;
    if handle.is_null() {
        // The verb ran without a process (unusual for `runas`); nothing to wait
        // for, and no exit code to forward.
        return Ok(0);
    }
    unsafe {
        WaitForSingleObject(handle, INFINITE);
    }
    let mut code: u32 = 0;
    let got = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe {
        CloseHandle(handle);
    }
    if got == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code as u8)
}

/// `ShellExecuteExW` reports failures through `GetLastError`, not an
/// `SE_ERR_*` instance value the way `ShellExecuteW` did.
fn launch_error(code: u32) -> io::Error {
    let reason = match code {
        1223 => "access denied (UAC declined?)".to_string(),
        other => format!("Win32 error {other}"),
    };
    io::Error::other(format!("elevation launch failed: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::{child_args, command_line, quote_arg};
    use std::ffi::{OsStr, OsString};

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

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
        assert_eq!(
            quote_arg(OsStr::new(r#"say "hi""#)),
            wide(r#""say \"hi\"""#)
        );
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

    #[test]
    fn child_args_strip_elevate_and_force_yes() {
        assert_eq!(
            child_args(os(&["add", r"C:\x", "--elevate"])),
            os(&["add", r"C:\x", "-y"])
        );
        assert_eq!(
            child_args(os(&["add", r"C:\x", "-e"])),
            os(&["add", r"C:\x", "-y"])
        );
        // An existing -y/--yes is not duplicated.
        assert_eq!(
            child_args(os(&["add", r"C:\x", "-y"])),
            os(&["add", r"C:\x", "-y"])
        );
        assert_eq!(
            child_args(os(&["add", r"C:\x", "--yes"])),
            os(&["add", r"C:\x", "--yes"])
        );
        // The elevated child carries no elevation flag, so it cannot respawn.
        assert!(
            !child_args(os(&["import", "f.json", "--elevate"]))
                .iter()
                .any(|a| a == OsStr::new("--elevate"))
        );
    }
}
