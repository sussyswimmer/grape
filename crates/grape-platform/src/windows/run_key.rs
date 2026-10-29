//! Whether Windows still starts Grape at sign-in, read straight from the
//! `Run` keys (`docs/PLATFORM_WINDOWS.md` → "Launch at login").
//!
//! The autostart plugin writes a value named after Grape under
//! `Software\Microsoft\Windows\CurrentVersion\Run`, in the current user's
//! hive or, when Grape runs elevated, the machine's. The uninstaller
//! deletes it and keeps `settings.json`, so after a reinstall the stored
//! "on" no longer matches what Windows does. The app asks [`run_value`]
//! whether the value is there at all. auto-launch's own `is_enabled` can't
//! tell: it's also false while Task Manager has Grape turned off, a choice
//! its `enable` would undo. So a value that's there is left alone, on or
//! off, and so is a missing one whose Task Manager record
//! (`Explorer\StartupApproved\Run`) still says off.
//!
//! Only reads, through `RegGetValueW`; the decisions are
//! [`crate::win_rules`]'s, tested on every OS.

use std::path::Path;

use ::windows::core::{w, HSTRING, PCWSTR};
use ::windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
};
use ::windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, REG_ROUTINE_FLAGS,
    REG_SZ, REG_VALUE_TYPE, RRF_NOEXPAND, RRF_RT_ANY,
};

use crate::win_rules::{
    approval_is_off, run_value_names, run_value_of, Approval, RunEntry, RunValue,
};

/// Where Windows keeps the programs it starts at sign-in.
const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// Task Manager's on/off record for each of them.
const APPROVED: PCWSTR =
    w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");

/// The current user's hive first, then the machine's.
const HIVES: [HKEY; 2] = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE];

/// How often a value that grew between the size query and the read is
/// read again.
const READ_TRIES: usize = 3;

/// What the `Run` keys say about the value `name` (the autostart plugin
/// names it after the product, "Grape"), for the executable `exe`. Any
/// thread may ask; nothing is written.
pub fn run_value(name: &str, exe: &Path) -> RunValue {
    let name = HSTRING::from(name);
    let entries = HIVES.map(|hive| match read_value(hive, RUN, &name) {
        Ok(None) => RunEntry::Absent,
        Ok(Some((kind, data))) => RunEntry::Present {
            names_exe: (kind == REG_SZ || kind == REG_EXPAND_SZ)
                && run_value_names(&utf16(&data), exe),
        },
        Err(_) => RunEntry::Unreadable,
    });
    let approvals = HIVES.map(|hive| match read_value(hive, APPROVED, &name) {
        Ok(None) => Approval::NotOff,
        Ok(Some((_, record))) if approval_is_off(&record) => Approval::Off,
        Ok(Some(_)) => Approval::NotOff,
        Err(_) => Approval::Unreadable,
    });
    run_value_of(entries, approvals)
}

/// The value `name` under `hive\subkey`, as its type and raw bytes, or
/// `None` when the key or the value isn't there. Strings come back
/// unexpanded and NUL-terminated (`RegGetValueW` adds the NUL).
fn read_value(
    hive: HKEY,
    subkey: PCWSTR,
    name: &HSTRING,
) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, WIN32_ERROR> {
    const FLAGS: REG_ROUTINE_FLAGS = REG_ROUTINE_FLAGS(RRF_RT_ANY.0 | RRF_NOEXPAND.0);
    for _ in 0..READ_TRIES {
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0_u32;
        // SAFETY: a predefined hive handle, a static NUL-terminated subkey,
        // a NUL-terminated name that outlives the call, and writable locals
        // for the type and size; no data buffer, so nothing else is written.
        let status = unsafe {
            RegGetValueW(
                hive,
                subkey,
                name,
                FLAGS,
                Some(&raw mut kind),
                None,
                Some(&raw mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        // Room for a NUL even when the value is empty.
        let mut data = vec![0_u8; usize::try_from(size).unwrap_or(0).max(2)];
        let mut size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        // SAFETY: as above, with a buffer of `size` writable bytes that
        // outlives the call; `size` is updated to the bytes written.
        let status = unsafe {
            RegGetValueW(
                hive,
                subkey,
                name,
                FLAGS,
                Some(&raw mut kind),
                Some(data.as_mut_ptr().cast()),
                Some(&raw mut size),
            )
        };
        match status {
            ERROR_SUCCESS => {
                data.truncate(usize::try_from(size).unwrap_or(0));
                return Ok(Some((kind, data)));
            }
            ERROR_FILE_NOT_FOUND => return Ok(None),
            // It grew since the size query: ask again.
            ERROR_MORE_DATA => {}
            other => return Err(other),
        }
    }
    Err(ERROR_MORE_DATA)
}

/// A registry string's UTF-16 bytes as text, up to its first NUL.
fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_no_program_uses_is_missing() {
        let name = format!("grape-platform test {} no such value", std::process::id());
        assert_eq!(
            run_value(&name, Path::new(r"C:\Grape\Grape.exe")),
            RunValue::Missing
        );
    }

    #[test]
    fn a_string_value_reads_back_as_text() {
        // Every Windows has it.
        let read = read_value(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            &HSTRING::from("ProductName"),
        );
        let Ok(Some((kind, data))) = read else {
            panic!("{read:?}");
        };
        assert_eq!(kind, REG_SZ);
        assert!(utf16(&data).starts_with("Windows"), "{}", utf16(&data));
        assert_eq!(
            read_value(
                HKEY_CURRENT_USER,
                w!(r"Software\grape-platform test no such key"),
                &HSTRING::from("x"),
            ),
            Ok(None)
        );
    }

    #[test]
    fn registry_text_stops_at_the_first_nul() {
        let bytes: Vec<u8> = "Grape.exe\0junk"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(utf16(&bytes), "Grape.exe");
        assert_eq!(utf16(&[]), "");
        assert_eq!(utf16(&[0x47]), "");
    }
}
