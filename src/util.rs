//! Small Win32 helpers: variable expansion and length limits.

/// Max size of a user-defined environment variable (Microsoft docs:
/// GetEnvironmentVariable / CreateEnvironmentBlock limit — not a registry
/// limit; the registry value-data limit is ~1 MB).
pub const MAX_ENV_VALUE: usize = 32_767;

/// Practical warning threshold for combined PATH length (command-line pain
/// starts around 2,048 chars).
pub const WARN_PATH_LEN: usize = 2_048;

/// PATH entry length worth surfacing by `check` (long-path support varies).
pub const LONG_PATH_FLAG: usize = 260;

/// Expand `%VAR%` references via ExpandEnvironmentStringsW. Returns the input
/// unchanged if expansion fails (unresolvable or overlong result).
pub fn expand(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; MAX_ENV_VALUE + 1];
    let n = unsafe {
        ExpandEnvironmentStringsW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize - 1])
    } else {
        s.to_string()
    }
}

/// True if `p` exists and is a directory. Does not resolve `%VAR%`.
pub fn dir_exists(p: &str) -> bool {
    std::path::Path::new(p).is_dir()
}

/// True if `p` contains a `%VAR%`-style reference.
pub fn has_var_ref(p: &str) -> bool {
    p.contains('%')
}
