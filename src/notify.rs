//! `WM_SETTINGCHANGE` broadcast so Explorer and new processes pick up
//! registry environment changes immediately (spec §4).

use windows_sys::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

/// Broadcast `WM_SETTINGCHANGE` with `lParam = "Environment"`. Best-effort:
/// hung windows must not block us.
pub fn broadcast_environment() {
    let msg: Vec<u16> = "Environment\0".encode_utf16().collect();
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            msg.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5_000,
            std::ptr::null_mut(),
        );
    }
}
