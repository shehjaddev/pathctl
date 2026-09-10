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
    // n is the size required including the NUL. n == 0 means expansion failed;
    // n > buf.len() means the result (or its NUL) does not fit the buffer —
    // slicing buf[..n-1] there would panic. Both fall back to the input,
    // matching the documented "unresolvable or overlong result" behavior.
    if n > 0 && (n as usize) <= buf.len() {
        String::from_utf16_lossy(&buf[..n as usize - 1])
    } else {
        s.to_string()
    }
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
    fn has_var_ref_needs_a_pair() {
        assert!(has_var_ref(r"%SystemRoot%\x"));
        assert!(has_var_ref(r"%ProgramFiles(x86)%\x"));
        assert!(!has_var_ref(r"C:\100%_coverage"));
        assert!(!has_var_ref(r"C:\tools"));
        assert!(!has_var_ref(r"C:\a%b\c"));
        assert!(!has_var_ref("%%"));
    }
}
