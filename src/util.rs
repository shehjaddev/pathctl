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
    // Most PATH entries hold no reference at all; there is nothing for Win32
    // to substitute, so skip the call and its buffers entirely.
    if !s.as_bytes().contains(&b'%') {
        return s.to_string();
    }
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    // Expand into a stack buffer first: entries are normally far shorter than
    // this, and a 64 KiB heap buffer per entry showed up as allocation churn
    // across `list`/`check` on a real PATH.
    let mut buf = [0u16; 512];
    let n = unsafe {
        ExpandEnvironmentStringsW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    // n is the size required including the NUL. n == 0 means expansion failed;
    // n > buf.len() means the result did not fit, and slicing buf[..n-1] there
    // would panic. An overlong result keeps the input, matching the documented
    // "unresolvable or overlong result" behaviour and bounding the retry.
    if n == 0 {
        return s.to_string();
    }
    let need = n as usize;
    if need > MAX_ENV_VALUE + 1 {
        return s.to_string();
    }
    if need <= buf.len() {
        return String::from_utf16_lossy(&buf[..need - 1]);
    }
    let mut retry = vec![0u16; need];
    let n = unsafe {
        ExpandEnvironmentStringsW(wide.as_ptr(), retry.as_mut_ptr(), retry.len() as u32)
    };
    if n == 0 || (n as usize) > retry.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&retry[..n as usize - 1])
}

/// True if `p` exists and is a directory. Does not resolve `%VAR%`.
pub fn dir_exists(p: &str) -> bool {
    std::path::Path::new(p).is_dir()
}

/// True if `p` contains a `%VAR%`-style reference (`%NAME%` with a
/// non-empty name). A lone `%` (e.g. `C:\100%_coverage`) is not a reference.
pub fn has_var_ref(p: &str) -> bool {
    let bytes = p.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'%' {
                // Variable names never contain path separators or `;`;
                // bail early so `C:\a%b\c` is not treated as a reference.
                if bytes[j] == b'\\' || bytes[j] == b'/' || bytes[j] == b';' {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'%' && j > i + 1 {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_passthrough_without_vars() {
        assert_eq!(expand(r"C:\tools"), r"C:\tools");
        assert_eq!(expand(""), "");
    }

    #[test]
    fn expand_resolves_real_var() {
        // Unique name; process-scoped on Windows, no cleanup needed.
        unsafe { std::env::set_var("PATHCTL_EXPAND_TEST", r"C:\resolved") };
        assert_eq!(expand("%PATHCTL_EXPAND_TEST%"), r"C:\resolved");
    }

    #[test]
    fn expand_retries_with_a_larger_buffer() {
        let big = "x".repeat(2_000);
        unsafe { std::env::set_var("PATHCTL_EXPAND_BIG", &big) };
        assert_eq!(expand("%PATHCTL_EXPAND_BIG%"), big);
        assert_eq!(
            expand(r"%PATHCTL_EXPAND_BIG%\tail"),
            format!("{big}\\tail")
        );
    }

    #[test]
    fn has_var_ref_needs_a_pair() {
        assert!(has_var_ref(r"%SystemRoot%\x"));
        assert!(has_var_ref(r"%ProgramFiles(x86)%\x"));
        assert!(!has_var_ref(r"C:\100%_coverage"));
        assert!(!has_var_ref(r"C:\tools"));
        assert!(!has_var_ref(r"C:\a%b\c"));
        assert!(!has_var_ref("%%"));
    }
}
