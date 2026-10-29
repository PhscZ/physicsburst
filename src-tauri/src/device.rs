//! Machine identifiers for the account service's device log.
//!
//! The launcher reads what a browser cannot: the machine's own unit identity
//! out of SMBIOS/DMI — the system UUID and the board's serial number. They are
//! collected once at startup and pinned into the page as JSON
//! (`__PB2_DEVICE`), which the game attaches to its account requests; the
//! account service keeps them beside the address log, so a moderator can see
//! that two accounts came from one machine.
//!
//! Only the unit identifiers are collected, because only they identify a unit.
//! A CPUID signature names a CPU *model* and a display adapter's PCI id names
//! a GPU *model*: millions of machines share either, so neither can tell two
//! accounts apart and neither is recorded.
//!
//! Best effort throughout: an identifier that cannot be read is left *out* of
//! the JSON rather than guessed at, so a container or an odd VM simply has
//! fewer rows to its name. Nothing here is a security boundary — a player owns
//! the machine and could fake any of it; this is evidence, not enforcement.

use serde_json::{json, Map, Value};

/// Every identifier this build knows how to read, as the JSON the page and the
/// account service both see. Keys are omitted when unavailable.
pub fn collect() -> Value {
    let mut out = Map::new();

    let (uuid, serial) = smbios_uuid_and_serial();
    if !uuid.is_empty() {
        out.insert("smbios_uuid".to_string(), json!(uuid));
    }
    if !serial.is_empty() {
        out.insert("bios_serial".to_string(), json!(serial));
    }

    Value::Object(out)
}

// ---- SMBIOS ---------------------------------------------------------------

#[cfg(windows)]
fn smbios_uuid_and_serial() -> (String, String) {
    // The firmware's own SMBIOS table. `GetSystemFirmwareTable` hands back a
    // RawSMBIOSData header followed by the table the BIOS published, which is
    // the same data Windows' WMI reads.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemFirmwareTable(
            signature: u32,
            table_id: u32,
            buffer: *mut u8,
            size: u32,
        ) -> u32;
    }

    const RSMB: u32 = 0x5253_4D42; // 'RSMB'

    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) };
    if size <= 8 {
        return (String::new(), String::new());
    }

    let mut buffer = vec![0u8; size as usize];
    let written = unsafe {
        GetSystemFirmwareTable(RSMB, 0, buffer.as_mut_ptr(), size)
    };
    if written == 0 || written > size {
        return (String::new(), String::new());
    }
    buffer.truncate(written as usize);

    // RawSMBIOSData: 4 bytes of version/revision, then a u32 length, then the
    // table itself.
    if buffer.len() < 8 {
        return (String::new(), String::new());
    }
    let table_len = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
    let table = buffer.get(8..(8 + table_len).min(buffer.len())).unwrap_or(&[]);
    smbios_system_identity(table)
}

#[cfg(target_os = "linux")]
fn smbios_uuid_and_serial() -> (String, String) {
    // The kernel exposes the DMI table's decoded fields directly. Some
    // distributions restrict them to root, in which case they read as empty
    // and the field is simply not reported.
    (
        read_trimmed("/sys/class/dmi/id/product_uuid"),
        read_trimmed("/sys/class/dmi/id/product_serial"),
    )
}

#[cfg(target_os = "macos")]
fn smbios_uuid_and_serial() -> (String, String) {
    // macOS has no DMI; these are the same two values under its own names.
    let dump = run("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
    (
        ioreg_value(&dump, "IOPlatformUUID"),
        ioreg_value(&dump, "IOPlatformSerialNumber"),
    )
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn smbios_uuid_and_serial() -> (String, String) {
    (String::new(), String::new())
}

/// Walks a raw SMBIOS table for the System Information structure (type 1) and
/// returns its UUID and serial number. Both are empty when the type is absent
/// or the firmware left a placeholder (`all 0x00` / `all 0xff`).
#[cfg_attr(not(windows), allow(dead_code))]
fn smbios_system_identity(table: &[u8]) -> (String, String) {
    let mut offset = 0usize;

    while offset + 4 <= table.len() {
        let kind = table[offset];
        let length = table[offset + 1] as usize;
        // A structure is 4 header bytes plus its formatted area; a length
        // below that, or one that runs past the table, means the walk is no
        // longer reading a structure.
        if length < 4 || offset + length > table.len() {
            break;
        }

        let formatted = &table[offset..offset + length];

        // The strings follow the formatted area, each NUL-terminated, and the
        // structure ends at the first empty one (two NULs in a row).
        let mut end = offset + length;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        let strings = table.get(offset + length..end).unwrap_or(&[]);

        if kind == 1 && formatted.len() >= 24 {
            let uuid = smbios_uuid(&formatted[8..24]);
            let serial = smbios_string(formatted[7], strings);
            return (uuid, serial);
        }

        offset = (end + 2).min(table.len());
    }

    (String::new(), String::new())
}

/// The 16-byte UUID field as text. SMBIOS stores the first three groups
/// little-endian, so they are byte-reversed to give the form the firmware and
/// every other tool report.
#[cfg_attr(not(windows), allow(dead_code))]
fn smbios_uuid(bytes: &[u8]) -> String {
    if bytes.len() < 16 {
        return String::new();
    }
    // "Not present" (all zero) and "not settable" (all ones) are placeholders,
    // not identities: boards that ship one would otherwise put every machine
    // of that model under the same id.
    if bytes.iter().all(|&b| b == 0) || bytes.iter().all(|&b| b == 0xff) {
        return String::new();
    }

    let uuid = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[3], bytes[2], bytes[1], bytes[0],
        bytes[5], bytes[4],
        bytes[7], bytes[6],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    );

    // A vendor's fixed id: firmware that ships one value for every board of a
    // model. Recording it would claim "these accounts are the same machine"
    // about machines that only share a vendor, which is worse than recording
    // nothing.
    const PLACEHOLDERS: [&str; 1] = [
        "03000200-0400-0500-0006-000700080009", // VMware's guests
    ];
    if PLACEHOLDERS.contains(&uuid.as_str()) {
        return String::new();
    }

    uuid
}

/// The Nth string of a structure's string area (1-based, as SMBIOS indexes
/// them).
#[cfg_attr(not(windows), allow(dead_code))]
fn smbios_string(index: u8, strings: &[u8]) -> String {
    if index == 0 {
        return String::new();
    }

    let mut current = 1u8;
    let mut start = 0usize;

    for (at, byte) in strings.iter().enumerate() {
        if *byte != 0 {
            continue;
        }
        if current == index {
            return String::from_utf8_lossy(&strings[start..at]).trim().to_string();
        }
        current += 1;
        start = at + 1;
    }

    // An area whose last string has no terminating NUL.
    if current == index {
        return String::from_utf8_lossy(&strings[start..]).trim().to_string();
    }

    String::new()
}

// ---- Platform helpers -----------------------------------------------------

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn run(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn ioreg_value(dump: &str, key: &str) -> String {
    for line in dump.lines() {
        let line = line.trim();
        if line.starts_with(&format!("\"{key}\"")) {
            if let Some(value) = quoted_value(line) {
                return value;
            }
        }
    }
    String::new()
}

/// The second quoted token of a line like `"Key" = "value"`.
#[cfg(target_os = "macos")]
fn quoted_value(line: &str) -> Option<String> {
    let after = line.split('=').nth(1)?.trim();
    let value = after.trim_matches('"').trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real firmware table, on the machine running the test. Ignored by
    // default because it prints machine identifiers; run with
    // `cargo test -- --ignored --nocapture` to see what this build reads.
    #[test]
    #[ignore]
    fn collects_identifiers() {
        let value = collect();
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
    }

    #[test]
    fn uuid_is_reordered_and_placeholder_is_dropped() {
        let mut bytes = [0u8; 16];
        bytes[0] = 0x44;
        bytes[1] = 0x45;
        bytes[2] = 0x4c;
        bytes[3] = 0x4c;
        bytes[4] = 0x43;
        bytes[5] = 0x4d;
        for (at, byte) in bytes.iter_mut().enumerate().skip(6) {
            *byte = at as u8;
        }
        assert_eq!(smbios_uuid(&bytes), "4c4c4544-4d43-0706-0809-0a0b0c0d0e0f");
        assert_eq!(smbios_uuid(&[0u8; 16]), "");
        assert_eq!(smbios_uuid(&[0xffu8; 16]), "");
    }

    #[test]
    fn walks_structures_to_the_system_one() {
        // Type 0 (BIOS) first, so the walk has to step over a structure with
        // strings before it finds type 1.
        let mut table: Vec<u8> = vec![0, 4, 0, 0]; // type 0, length 4, no strings
        table.extend_from_slice(&[0, 0]); // the empty string that ends it

        let mut system = vec![1u8, 27, 1, 0]; // type 1, length 27
        // String numbers: manufacturer, product, version, serial — which is
        // the order the strings below are written in.
        system.extend_from_slice(&[1, 2, 3, 4]);
        // The firmware's bytes for the UUID below, first three groups
        // little-endian, which is how SMBIOS stores them.
        let uuid = [
            0x44u8, 0x45, 0x4c, 0x4c, 0x43, 0x4d, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        system.extend_from_slice(&uuid);
        system.extend_from_slice(&[0, 0, 0]); // wake-up type and the rest
        table.extend_from_slice(&system);
        table.extend_from_slice(b"Acme\0Server\0v1\0SN-1234\0\0");

        let (uuid, serial) = smbios_system_identity(&table);
        assert_eq!(uuid, "4c4c4544-4d43-0706-0809-0a0b0c0d0e0f");
        assert_eq!(serial, "SN-1234");
    }

    #[test]
    fn missing_system_structure_gives_nothing() {
        let table: Vec<u8> = vec![0, 4, 0, 0, 0, 0];
        assert_eq!(smbios_system_identity(&table), (String::new(), String::new()));
    }
}
