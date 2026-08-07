//! Thin `winreg` wrapper: read/write PATH and env vars, type-preserving.
//!
//! Storage (verified against Microsoft docs, 2026-08-07):
//! - user PATH:   `HKCU\Environment\Path`
//! - system PATH: `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\Path`
//!
//! Every write preserves the existing registry value type (`REG_EXPAND_SZ`
//! stays `REG_EXPAND_SZ`) — the core differentiator from .NET tooling, which
//! flattens expandable values to `REG_SZ` (dotnet/runtime#89695, unfixed by
//! design). Never goes through `setx` (1024-char crop, documented data loss).

use std::io;
use winreg::enums::{KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};
use winreg::{RegKey, RegValue, HKLM, HKCU};

pub use winreg::enums::RegType;

pub const USER_KEY_PATH: &str = "Environment";
pub const SYSTEM_KEY_PATH: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const TEST_KEY_PREFIX: &str = r"Software\pathctl-test";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }
}

/// A registry value plus its type.
#[derive(Debug, Clone)]
pub struct PathValue {
    pub raw: String,
    pub ty: RegType,
}

/// Registry access. `test` redirects all scopes under
/// `HKCU\Software\pathctl-test\{user,system}` so tests never touch the real
/// PATH (spec §8).
#[derive(Debug, Clone)]
pub struct Registry {
    test: bool,
}

impl Registry {
    /// Prod instance. `PATHCTL_TEST_REG` redirects writes to a scratch key
    /// under `HKCU\Software\pathctl-test…` (CLI tests).
    pub fn new() -> Self {
        Self { test: false }
    }

    /// Explicit test instance (unit tests; no env race). CLI tests instead
    /// spawn the binary with `PATHCTL_TEST_REG`.
    #[cfg(test)]
    pub fn test() -> Self {
        Self { test: true }
    }

    fn key_path(&self, scope: Scope) -> String {
        if let Some(v) = std::env::var_os("PATHCTL_TEST_REG") {
            // CLI tests: `PATHCTL_TEST_REG=1` → default key; any other value
            // → `Software\pathctl-test-<value>`, giving each test an isolated key.
            let suffix = if v == "1" {
                String::new()
            } else {
                format!("-{}", v.to_string_lossy())
            };
            format!(r"{TEST_KEY_PREFIX}{suffix}\{}", scope.label())
        } else if self.test {
            format!(r"{TEST_KEY_PREFIX}\{}", scope.label())
        } else {
            match scope {
                Scope::User => USER_KEY_PATH.to_string(),
                Scope::System => SYSTEM_KEY_PATH.to_string(),
            }
        }
    }

    fn hive(&self, scope: Scope) -> &'static RegKey {
        if self.test || std::env::var_os("PATHCTL_TEST_REG").is_some() {
            // Test mode redirects every scope under HKCU, so the hive must
            // follow (system scope would otherwise hit real HKLM keys).
            HKCU
        } else {
            match scope {
                Scope::User => HKCU,
                Scope::System => HKLM,
            }
        }
    }

    fn open_read(&self, scope: Scope) -> io::Result<RegKey> {
        self.open_with(scope, KEY_READ)
    }

    fn open(&self, scope: Scope) -> io::Result<RegKey> {
        self.open_with(scope, KEY_READ | KEY_WRITE)
    }

    fn open_with(&self, scope: Scope, access: u32) -> io::Result<RegKey> {
        let hive = self.hive(scope);
        match hive.open_subkey_with_flags(self.key_path(scope), access) {
            Ok(k) => Ok(k),
            // Missing key on a fresh profile: create it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                hive.create_subkey(self.key_path(scope)).map(|(k, _)| k)
            }
            Err(e) => Err(e),
        }
    }

    fn read_value(&self, scope: Scope, name: &str) -> io::Result<Option<PathValue>> {
        match self.open_read(scope)?.get_raw_value(name) {
            Ok(v) => Ok(Some(PathValue {
                raw: decode(&v.bytes),
                ty: v.vtype,
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_value(&self, scope: Scope, name: &str, value: &str, ty: RegType) -> io::Result<()> {
        self.open(scope)?.set_raw_value(
            name,
            &RegValue {
                bytes: encode(value).into(),
                vtype: ty,
            },
        )
    }

    pub fn read_path(&self, scope: Scope) -> io::Result<Option<PathValue>> {
        self.read_value(scope, "Path")
    }

    pub fn write_path(&self, scope: Scope, value: &str, ty: RegType) -> io::Result<()> {
        self.write_value(scope, "Path", value, ty)
    }

    pub fn read_var(&self, scope: Scope, name: &str) -> io::Result<Option<PathValue>> {
        self.read_value(scope, name)
    }

    pub fn write_var(&self, scope: Scope, name: &str, value: &str, ty: RegType) -> io::Result<()> {
        self.write_value(scope, name, value, ty)
    }

    pub fn delete_var(&self, scope: Scope, name: &str) -> io::Result<()> {
        match self.open(scope)?.delete_value(name) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()), // idempotent
            Err(e) => Err(e),
        }
    }

    /// All values under a scope key (export).
    pub fn enum_all(&self, scope: Scope) -> io::Result<Vec<(String, PathValue)>> {
        let mut out = Vec::new();
        for item in self.open_read(scope)?.enum_values() {
            let (name, v) = item?;
            out.push((
                name,
                PathValue {
                    raw: decode(&v.bytes),
                    ty: v.vtype,
                },
            ));
        }
        Ok(out)
    }
}

/// Default type for a *new* PATH value: expandable, like Windows ships it.
pub fn default_path_type() -> RegType {
    REG_EXPAND_SZ
}

/// Default type for a *new* general env var: plain string.
pub fn default_var_type() -> RegType {
    REG_SZ
}

/// Stable u32 form for JSON (snapshots, exports).
pub fn reg_type_to_u32(t: RegType) -> u32 {
    t as isize as u32
}

/// Reconstruct a `RegType` from its u32 form; unknown values fall back to
/// `REG_SZ` (defensive: future Windows value types we don't know). The
/// numeric values are the stable Win32 registry value-type constants.
pub fn reg_type_from_u32(v: u32) -> RegType {
    use winreg::enums::RegType;
    match v as isize {
        0 => RegType::REG_NONE,
        1 => RegType::REG_SZ,
        2 => RegType::REG_EXPAND_SZ,
        3 => RegType::REG_BINARY,
        4 => RegType::REG_DWORD,
        5 => RegType::REG_DWORD_BIG_ENDIAN,
        6 => RegType::REG_LINK,
        7 => RegType::REG_MULTI_SZ,
        8 => RegType::REG_RESOURCE_LIST,
        9 => RegType::REG_FULL_RESOURCE_DESCRIPTOR,
        10 => RegType::REG_RESOURCE_REQUIREMENTS_LIST,
        11 => RegType::REG_QWORD,
        _ => RegType::REG_SZ,
    }
}

/// REG_* values are NUL-terminated UTF-16LE.
fn encode(s: &str) -> Vec<u8> {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    let mut bytes = Vec::with_capacity(wide.len() * 2);
    for u in wide {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes
}

fn decode(bytes: &[u8]) -> String {
    let mut wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    while wide.last() == Some(&0) {
        wide.pop();
    }
    String::from_utf16_lossy(&wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_preserving_roundtrip_on_test_key() {
        let reg = Registry::test();
        let before = reg.read_path(Scope::User).unwrap();
        let value = r"%SystemRoot%\test;%ProgramFiles%\test";
        reg.write_path(Scope::User, value, REG_EXPAND_SZ).unwrap();
        let got = reg.read_path(Scope::User).unwrap().unwrap();
        assert_eq!(got.raw, value);
        assert_eq!(got.ty, REG_EXPAND_SZ, "type must be preserved");
        match before {
            Some(b) => reg.write_path(Scope::User, &b.raw, b.ty).unwrap(),
            None => {
                let _ = reg.delete_var(Scope::User, "Path");
            }
        }
    }

    #[test]
    fn literal_percent_entries_survive_roundtrip() {
        let reg = Registry::test();
        let name = "PATHCTL_TEST_VAR";
        reg.write_var(Scope::User, name, r"%SystemRoot%\x", REG_EXPAND_SZ)
            .unwrap();
        let got = reg.read_var(Scope::User, name).unwrap().unwrap();
        assert_eq!(got.raw, r"%SystemRoot%\x");
        assert_eq!(got.ty, REG_EXPAND_SZ);
        reg.delete_var(Scope::User, name).unwrap();
        assert!(reg.read_var(Scope::User, name).unwrap().is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        assert_eq!(decode(&encode(r"C:\tools")), r"C:\tools");
        assert_eq!(decode(&encode("")), "");
    }
}
