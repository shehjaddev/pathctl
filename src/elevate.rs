//! Relaunch self elevated (UAC) for system-scope writes (spec §4).

use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Relaunch the current binary with the `runas` verb, forwarding argv[1..].
/// Note: args are re-joined with spaces, so quoted multi-word arguments are
/// not round-tripped faithfully — our flags are simple, so this is acceptable.
pub fn relaunch_elevated() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let args_wide: Vec<u16> = args
        .join(" ")
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
    // ShellExecuteW returns an HINSTANCE; values <= 32 are error codes.
    if res as isize > 32 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
