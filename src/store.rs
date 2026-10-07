//! Bounded, validated persistence for controller state.
//!
//! Files are loaded completely and validated before publication.
//! Only NotFound means "not initialized"; other I/O errors are fatal.
//!
//! Paths and ancestor directories must be administrator controlled.
//! O_NOFOLLOW protects the final component, not malicious changes to
//! ancestor directories.

use crate::{
    error::{AppError, AppResult},
    usb::Gen5MapEntry,
};

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use nix::libc;

pub type UsbMappings = HashMap<(String, String, u8), String>;
pub type Gen5Mappings = HashMap<String, Gen5MapEntry>;
pub type UartMappings = HashMap<String, UartMapping>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UartMapping {
    pub mac: String,
    pub generation: u8,
    pub tty: String,
    pub vid: u16,
    pub pid: u16,
    pub usb_serial: Option<String>,
    pub interface: u8,
    pub topology: String,
    pub connection: String,
    pub relay_serial: String,
    pub channel: u8,
}

pub const MAX_STATE_BYTES: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;
const MAX_ROW_BYTES: usize = 1024;
const MAX_UID_BYTES: usize = 128;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::Msg(message.into())
}

/// Open without following a final symlink.
///
/// O_NONBLOCK prevents hanging on a FIFO before metadata rejects it.
fn open_regular(path: &Path) -> AppResult<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(invalid(format!("cannot open {}: {error}", path.display())));
        }
    };

    if !file.metadata()?.is_file() {
        return Err(invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }

    Ok(Some(file))
}

pub fn read_optional_text(path: &Path, limit: usize) -> AppResult<Option<String>> {
    let Some(file) = open_regular(path)? else {
        tracing::debug!(
            path = %path.display(),
            limit,
            "state file absent; treating it as empty/default mapping state"
        );
        return Ok(None);
    };

    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;

    if bytes.len() > limit {
        tracing::error!(
            path = %path.display(),
            limit,
            actual_size = bytes.len(),
            "state file exceeds configured size limit"
        );
        return Err(invalid(format!(
            "{} exceeds its {}-byte limit",
            path.display(),
            limit
        )));
    }

    let text = String::from_utf8(bytes)
        .map_err(|_| invalid(format!("{} is not valid UTF-8", path.display())))?;

    tracing::debug!(
        path = %path.display(),
        bytes = text.len(),
        "state file read successfully"
    );
    Ok(Some(text))
}

pub fn read_required_text(path: &Path, limit: usize) -> AppResult<String> {
    match read_optional_text(path, limit)? {
        Some(text) => {
            tracing::debug!(path = %path.display(), bytes = text.len(), "required configuration file loaded");
            Ok(text)
        }
        None => {
            tracing::error!(path = %path.display(), limit, "required file missing; failing startup");
            Err(invalid(format!(
                "required file is missing: {}",
                path.display()
            )))
        }
    }
}

/// Accept exactly:
/// - aabbccddeeff
/// - aa:bb:cc:dd:ee:ff
/// - aa-bb-cc-dd-ee-ff
///
/// Do not erase arbitrary separators or whitespace before validation.
pub fn checked_mac(value: &str) -> AppResult<String> {
    let bytes = value.as_bytes();

    let valid = match bytes.len() {
        12 => bytes.iter().all(u8::is_ascii_hexdigit),
        17 => {
            let separator = bytes[2];
            matches!(separator, b':' | b'-')
                && bytes.iter().enumerate().all(|(index, byte)| {
                    if index % 3 == 2 {
                        *byte == separator
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        }
        _ => false,
    };

    if !valid {
        return Err(invalid("invalid MAC address format"));
    }

    Ok(value
        .chars()
        .filter(|character| !matches!(character, ':' | '-'))
        .map(|character| character.to_ascii_lowercase())
        .collect())
}

pub fn validate_serial(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > 255
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b',' | b'"'))
    {
        return Err(invalid(
            "relay serial must be 1..255 visible ASCII bytes without comma or quote",
        ));
    }

    Ok(())
}

/// Validate the path syntax without requiring hardware to be attached.
///
/// Current discovery persists canonical /dev/ttyUSBn or /dev/ttyACMn names.
/// Other path forms must be migrated explicitly rather than silently
/// canonicalized during startup.
pub fn validate_tty(value: &str) -> AppResult<()> {
    let valid = ["/dev/ttyUSB", "/dev/ttyACM"].iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.bytes().all(|byte| byte.is_ascii_digit())
                && suffix.parse::<u32>().is_ok()
                && (suffix == "0" || !suffix.starts_with('0'))
        })
    });

    if !valid {
        return Err(invalid("invalid persisted USB/ACM TTY pathname"));
    }

    Ok(())
}

/// The supplied sources do not specify a richer controller-ID grammar.
///
/// Permit the common UUID/token characters without admitting whitespace,
/// control characters, URL delimiters, or unbounded values.
pub fn validate_uid(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > MAX_UID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid(
            "controllerId must be 1..128 ASCII letters/digits or '-', '_', '.'",
        ));
    }

    Ok(())
}

pub fn load_uid(path: &Path) -> AppResult<Option<String>> {
    let Some(text) = read_optional_text(path, MAX_UID_BYTES + 2)? else {
        tracing::debug!(path = %path.display(), "UID not present; controller not yet confirmed");
        return Ok(None);
    };

    // Permit one conventional terminal newline, not arbitrary trimming.
    let uid = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);

    validate_uid(uid)
        .map_err(|error| invalid(format!("invalid UID file {}: {error}", path.display())))?;

    tracing::info!(path = %path.display(), uid = %uid, "loaded persisted controller UID");
    Ok(Some(uid.to_owned()))
}

fn parse_rows(text: &str, columns: usize) -> AppResult<Vec<Vec<&str>>> {
    if text.len() > MAX_STATE_BYTES {
        return Err(invalid("mapping file exceeds 1 MiB"));
    }

    let mut rows = Vec::new();

    for (index, line) in text.lines().enumerate() {
        if index >= MAX_ROWS {
            return Err(invalid("mapping file exceeds 4096 rows"));
        }

        if line.is_empty() || line.len() > MAX_ROW_BYTES || line.chars().any(char::is_control) {
            return Err(invalid(format!(
                "mapping line {} is empty, oversized, or contains controls",
                index + 1
            )));
        }

        let fields: Vec<_> = line.split(',').collect();

        if fields.len() != columns
            || fields
                .iter()
                .any(|field| field.is_empty() || field.trim() != *field || field.contains('"'))
        {
            return Err(invalid(format!(
                "mapping line {} has invalid unquoted CSV fields",
                index + 1
            )));
        }

        rows.push(fields);
    }

    // A genuinely empty file represents an empty mapping set.
    Ok(rows)
}

pub fn parse_usb(text: &str) -> AppResult<UsbMappings> {
    let mut mappings = UsbMappings::new();
    let mut macs = HashSet::new();
    let mut ttys = HashSet::new();
    let mut relays = HashSet::new();

    for fields in parse_rows(text, 4)? {
        let tty = fields[0];
        let mac = checked_mac(fields[1])?;
        let serial = fields[2];

        validate_tty(tty)?;
        validate_serial(serial)?;

        // One decimal digit; no atoi-like defaulting or partial parsing.
        let channel = match fields[3].as_bytes() {
            [value @ b'0'..=b'7'] => *value - b'0',
            _ => return Err(invalid("persisted relay channel must be 0..7")),
        };

        if !macs.insert(mac.clone())
            || !ttys.insert(tty.to_owned())
            || !relays.insert((serial.to_owned(), channel))
        {
            return Err(invalid(
                "duplicate board, UART, or relay/channel in USB mappings",
            ));
        }

        mappings.insert((mac, serial.to_owned(), channel), tty.to_owned());
    }

    Ok(mappings)
}

pub fn parse_gen5(text: &str) -> AppResult<Gen5Mappings> {
    let mut mappings = Gen5Mappings::new();
    let mut devices = HashSet::new();

    for fields in parse_rows(text, 3)? {
        let uart = fields[0];
        let power = fields[1];
        let mac = checked_mac(fields[2])?;

        validate_tty(uart)?;
        validate_tty(power)?;

        if mappings.contains_key(&mac)
            || !devices.insert(uart.to_owned())
            || !devices.insert(power.to_owned())
        {
            return Err(invalid(
                "duplicate board or shared device role in Gen5 mappings",
            ));
        }

        mappings.insert(
            mac.clone(),
            Gen5MapEntry {
                uart: uart.to_owned(),
                power: power.to_owned(),
                mac,
            },
        );
    }

    Ok(mappings)
}

pub fn parse_uart(text: &str) -> AppResult<UartMappings> {
    let mut mappings = UartMappings::new();
    let mut ttys = HashSet::new();

    for fields in parse_rows(text, 11)? {
        let mac = checked_mac(fields[0])?;
        let generation = fields[1]
            .parse::<u8>()
            .ok()
            .filter(|generation| matches!(generation, 3..=5))
            .ok_or_else(|| invalid("UART mapping generation must be 3, 4, or 5"))?;
        validate_tty(fields[2])?;
        let parse_hex = |value: &str, field: &str| {
            (value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| u16::from_str_radix(value, 16).ok())
                .flatten()
                .ok_or_else(|| invalid(format!("UART mapping {field} must be four hex digits")))
        };
        let vid = parse_hex(fields[3], "VID")?;
        let pid = parse_hex(fields[4], "PID")?;
        let usb_serial = (fields[5] != "-").then(|| fields[5].to_owned());
        if usb_serial.as_ref().is_some_and(|value| {
            value.len() > 255
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b',' | b'"'))
        }) {
            return Err(invalid("invalid UART USB serial"));
        }
        let interface = fields[6]
            .parse::<u8>()
            .map_err(|_| invalid("UART interface must be 0..255"))?;
        if !fields[7].starts_with('/') || fields[7].chars().any(char::is_control) {
            return Err(invalid("UART topology must be an absolute, printable path"));
        }
        if !matches!(fields[8], "standalone" | "hub") {
            return Err(invalid("UART connection must be standalone or hub"));
        }
        let relay_serial = if generation != 5 { fields[9] } else { "-" };
        if generation != 5 {
            validate_serial(relay_serial)?;
        } else if fields[9] != "-" || fields[10] != "-" {
            return Err(invalid(
                "Gen5 UART mappings cannot contain relay information",
            ));
        }
        let channel = if generation == 5 {
            0
        } else {
            fields[10]
                .parse::<u8>()
                .ok()
                .filter(|channel| *channel <= 7)
                .ok_or_else(|| invalid("UART relay channel must be 0..7"))?
        };

        if mappings.contains_key(&mac) || !ttys.insert(fields[2].to_owned()) {
            return Err(invalid("duplicate target MAC or UART TTY in mappings"));
        }

        mappings.insert(
            mac.clone(),
            UartMapping {
                mac,
                generation,
                tty: fields[2].to_owned(),
                vid,
                pid,
                usb_serial,
                interface,
                topology: fields[7].to_owned(),
                connection: fields[8].to_owned(),
                relay_serial: relay_serial.to_owned(),
                channel,
            },
        );
    }

    Ok(mappings)
}

/// Validate both persisted mapping files together.
///
/// No sysfs access occurs here. Offline or powered-off devices must not
/// prevent loading structurally valid persisted state.
pub fn validate_snapshot(usb: &UsbMappings, gen5: &Gen5Mappings) -> AppResult<()> {
    tracing::debug!(
        usb_count = usb.len(),
        gen5_count = gen5.len(),
        "validating persisted USB and Gen5 mappings"
    );

    // Reparse the serialized form to enforce the same rules for in-memory
    // writes as for startup reads.
    let parsed_usb = parse_usb(&encode_usb(usb)?)?;
    let parsed_gen5 = parse_gen5(&encode_gen5(gen5)?)?;

    let mut used_devices = HashSet::new();
    let mut used_macs = HashSet::new();

    for ((mac, _, _), tty) in &parsed_usb {
        if !used_devices.insert(tty.clone()) || !used_macs.insert(mac.clone()) {
            tracing::warn!(mac = %mac, tty = %tty, "conflicting persisted USB mapping detected");
            return Err(invalid("conflicting persisted USB mapping"));
        }
    }

    for (mac, entry) in &parsed_gen5 {
        if !used_macs.insert(mac.clone())
            || !used_devices.insert(entry.uart.clone())
            || !used_devices.insert(entry.power.clone())
        {
            tracing::warn!(mac = %mac, uart = %entry.uart, power = %entry.power, "Gen5 mapping shares a board or device pathname");
            return Err(invalid("mapping files share a board or device pathname"));
        }
    }

    tracing::info!(
        usb_count = parsed_usb.len(),
        gen5_count = parsed_gen5.len(),
        "persisted mapping snapshot validated"
    );
    Ok(())
}

pub fn load_mappings(usb_path: &Path, gen5_path: &Path) -> AppResult<(UsbMappings, Gen5Mappings)> {
    let usb_text = read_optional_text(usb_path, MAX_STATE_BYTES)?.unwrap_or_default();
    let gen5_text = read_optional_text(gen5_path, MAX_STATE_BYTES)?.unwrap_or_default();

    if usb_text.is_empty() {
        tracing::info!(path = %usb_path.display(), "USB mapping file absent or empty; using empty persisted USB mapping state");
    }
    if gen5_text.is_empty() {
        tracing::info!(path = %gen5_path.display(), "Gen5 mapping file absent or empty; using empty persisted Gen5 mapping state");
    }

    tracing::debug!(
        usb_path = %usb_path.display(),
        gen5_path = %gen5_path.display(),
        usb_bytes = usb_text.len(),
        gen5_bytes = gen5_text.len(),
        "loading persisted mappings and validating them"
    );

    let usb = parse_usb(&usb_text).map_err(|error| {
        tracing::error!(path = %usb_path.display(), error = %error, "USB mapping file is invalid");
        invalid(format!("invalid {}: {error}", usb_path.display()))
    })?;

    let gen5 = parse_gen5(&gen5_text)
        .map_err(|error| {
            tracing::error!(path = %gen5_path.display(), error = %error, "Gen5 mapping file is invalid");
            invalid(format!("invalid {}: {error}", gen5_path.display()))
        })?;

    validate_snapshot(&usb, &gen5)?;

    tracing::info!(
        usb_mappings = usb.len(),
        gen5_mappings = gen5.len(),
        "persisted mappings loaded and verified"
    );
    Ok((usb, gen5))
}

pub fn load_uart_mappings(path: &Path) -> AppResult<UartMappings> {
    let text = read_optional_text(path, MAX_STATE_BYTES)?.unwrap_or_default();
    parse_uart(&text).map_err(|error| invalid(format!("invalid {}: {error}", path.display())))
}

pub fn encode_usb(mappings: &UsbMappings) -> AppResult<String> {
    let mut rows = Vec::with_capacity(mappings.len());

    for ((mac, serial, channel), tty) in mappings {
        validate_tty(tty)?;
        validate_serial(serial)?;

        if checked_mac(mac)? != *mac || *channel > 7 {
            return Err(invalid("in-memory USB mapping is not canonical"));
        }

        rows.push(format!("{tty},{mac},{serial},{channel}\n"));
    }

    rows.sort_unstable();
    Ok(rows.concat())
}

pub fn encode_gen5(mappings: &Gen5Mappings) -> AppResult<String> {
    let mut rows = Vec::with_capacity(mappings.len());

    for (mac, entry) in mappings {
        validate_tty(&entry.uart)?;
        validate_tty(&entry.power)?;

        if checked_mac(mac)? != *mac || entry.mac != *mac {
            return Err(invalid("in-memory Gen5 mapping identity is inconsistent"));
        }

        rows.push(format!("{},{},{mac}\n", entry.uart, entry.power));
    }

    rows.sort_unstable();
    Ok(rows.concat())
}

pub fn encode_uart(mappings: &UartMappings) -> AppResult<String> {
    let mut rows = Vec::with_capacity(mappings.len());

    for (mac, mapping) in mappings {
        if mac != &mapping.mac {
            return Err(invalid("UART mapping key does not match its MAC"));
        }
        for value in [
            mapping.usb_serial.as_deref().unwrap_or("-"),
            mapping.topology.as_str(),
            mapping.connection.as_str(),
            mapping.relay_serial.as_str(),
        ] {
            if value.is_empty() || value.contains(',') || value.chars().any(char::is_control) {
                return Err(invalid("UART mapping contains an invalid CSV field"));
            }
        }
        rows.push(format!(
            "{},{},{},{:04x},{:04x},{},{},{},{},{},{}\n",
            mapping.mac,
            mapping.generation,
            mapping.tty,
            mapping.vid,
            mapping.pid,
            mapping.usb_serial.as_deref().unwrap_or("-"),
            mapping.interface,
            mapping.topology,
            mapping.connection,
            if mapping.generation == 5 {
                "-"
            } else {
                &mapping.relay_serial
            },
            if mapping.generation == 5 {
                "-".to_owned()
            } else {
                mapping.channel.to_string()
            },
        ));
    }

    rows.sort_unstable();
    let encoded = rows.concat();
    parse_uart(&encoded)?;
    Ok(encoded)
}

/// Atomically replace one file, then attempt to synchronize its directory.
///
/// Commit point: successful rename by persist().
///
/// A directory-sync failure after rename is logged but not returned as
/// "write failed": the new data is already visible, so callers must publish
/// the matching in-memory value. Crash durability is uncertain in that case.
pub fn atomic_replace(path: &Path, contents: &[u8]) -> AppResult<()> {
    if contents.len() > MAX_STATE_BYTES {
        tracing::error!(path = %path.display(), size = contents.len(), "state write exceeds limit");
        return Err(invalid("state write exceeds 1 MiB"));
    }

    tracing::debug!(path = %path.display(), bytes = contents.len(), "writing state atomically");

    let parent = path
        .parent()
        .ok_or_else(|| invalid("state pathname has no parent directory"))?;

    // Reject a pre-existing symlink or special file.
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            tracing::warn!(path = %path.display(), "refusing to replace non-regular state file");
            return Err(invalid(format!(
                "{} must be a regular file",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Parent creation is intentionally an installation responsibility.
    let directory = File::open(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;

    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;

    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;

    temporary.persist(path).map_err(|error| error.error)?;

    if let Err(error) = directory.sync_all() {
        tracing::error!(
            path = %path.display(),
            %error,
            "state committed but directory sync failed; crash durability uncertain"
        );
    }

    tracing::info!(path = %path.display(), bytes = contents.len(), "state file atomically replaced");
    Ok(())
}

pub fn remove_committed(path: &Path) -> AppResult<()> {
    tracing::debug!(path = %path.display(), "removing persisted state file");

    let parent = path
        .parent()
        .ok_or_else(|| invalid("state pathname has no parent directory"))?;
    let directory = File::open(parent)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            tracing::warn!(path = %path.display(), "refusing to remove non-regular state file");
            return Err(invalid("refusing to remove non-regular state file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "state file was already absent; no-op removal");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if let Err(error) = directory.sync_all() {
        tracing::error!(
            path = %path.display(),
            %error,
            "state removal committed but directory sync failed"
        );
    }

    tracing::info!(path = %path.display(), "persisted state file removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn mac_parser_accepts_only_supported_complete_forms() {
        for value in ["AABBCCDDEEFF", "AA:BB:CC:DD:EE:FF", "AA-BB-CC-DD-EE-FF"] {
            assert_eq!(checked_mac(value).unwrap(), "aabbccddeeff");
        }

        for value in [
            "aa:bb:cc",
            "aa-bb:cc-dd-ee-ff",
            "aa bb cc dd ee ff",
            "aabbccddeeff00",
            "aabbccddeefg",
            " aabbccddeeff",
        ] {
            assert!(checked_mac(value).is_err());
        }
    }

    #[test]
    fn empty_mapping_files_are_valid_but_blank_records_are_not() {
        assert!(parse_usb("").unwrap().is_empty());
        assert!(parse_gen5("").unwrap().is_empty());
        assert!(parse_usb("\n").is_err());
        assert!(parse_gen5(" \n").is_err());
    }

    #[test]
    fn usb_mapping_round_trip_is_canonical() {
        let parsed = parse_usb("/dev/ttyUSB2,AA:BB:CC:DD:EE:FF,RELAY-A,3\r\n").unwrap();

        assert_eq!(
            encode_usb(&parsed).unwrap(),
            "/dev/ttyUSB2,aabbccddeeff,RELAY-A,3\n"
        );
    }

    #[test]
    fn uart_mapping_round_trip_preserves_topology_and_connection() {
        let mapping = UartMapping {
            mac: "aabbccddeeff".into(),
            generation: 4,
            tty: "/dev/ttyUSB2".into(),
            vid: 0x0403,
            pid: 0x6010,
            usb_serial: Some("UART-A".into()),
            interface: 1,
            topology: "/sys/devices/pci0000:00/usb1/1-2/1-2.3".into(),
            connection: "hub".into(),
            relay_serial: "RELAY-A".into(),
            channel: 0,
        };
        let mappings = UartMappings::from([(mapping.mac.clone(), mapping.clone())]);

        let encoded = encode_uart(&mappings).unwrap();

        assert_eq!(
            parse_uart(&encoded).unwrap().get(&mapping.mac),
            Some(&mapping)
        );
    }

    #[test]
    fn gen5_uart_mapping_records_generation_without_relay_fields() {
        let text =
            "aabbccddeeff,5,/dev/ttyACM0,1234,abcd,-,0,/sys/devices/usb1/1-2,standalone,-,-\n";

        let parsed = parse_uart(text).unwrap();
        let mapping = parsed.get("aabbccddeeff").unwrap();

        assert_eq!(mapping.generation, 5);
        assert_eq!(mapping.relay_serial, "-");
        assert_eq!(encode_uart(&parsed).unwrap(), text);
        assert!(parse_uart(
            "aabbccddeeff,5,/dev/ttyACM0,1234,abcd,-,0,/sys/devices/usb1/1-2,standalone,RELAY,0\n"
        )
        .is_err());
    }

    #[test]
    fn malformed_channel_never_defaults_to_zero() {
        for channel in ["", "-1", "8", "03", "0x1", "one", "1tail"] {
            assert!(parse_usb(&format!("/dev/ttyUSB0,aabbccddeeff,R,{channel}\n")).is_err());
        }
    }

    #[test]
    fn duplicates_and_shared_roles_are_rejected() {
        assert!(parse_usb(
            "/dev/ttyUSB0,aabbccddeeff,R,0\n\
             /dev/ttyUSB1,aabbccddeeff,R,1\n"
        )
        .is_err());

        assert!(parse_usb(
            "/dev/ttyUSB0,aabbccddeeff,R,0\n\
             /dev/ttyUSB1,aabbccddee00,R,0\n"
        )
        .is_err());

        assert!(parse_gen5("/dev/ttyUSB0,/dev/ttyUSB0,aabbccddeeff\n").is_err());

        assert!(parse_gen5(
            "/dev/ttyUSB0,/dev/ttyUSB1,aabbccddeeff\n\
             /dev/ttyUSB2,/dev/ttyUSB0,aabbccddee00\n"
        )
        .is_err());
    }

    #[test]
    fn unsafe_or_noncanonical_tty_paths_are_rejected() {
        for path in [
            "/tmp/ttyUSB0",
            "/dev/../dev/ttyUSB0",
            "/dev/ttyUSB",
            "/dev/ttyUSB01",
            "/dev/ttyUSB0/extra",
            "/dev/null",
        ] {
            assert!(validate_tty(path).is_err());
        }
    }

    #[test]
    fn uid_is_bounded_and_not_silently_trimmed() {
        assert!(validate_uid("controller_123-abc.def").is_ok());
        for value in ["", " has-space", "line\nbreak", "id?admin=true"] {
            assert!(validate_uid(value).is_err());
        }
        assert!(validate_uid(&"x".repeat(129)).is_err());
    }

    #[test]
    fn missing_is_optional_but_symlinks_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");

        assert!(read_optional_text(&path, 32).unwrap().is_none());

        let target = directory.path().join("target");
        fs::write(&target, "data").unwrap();
        symlink(&target, &path).unwrap();

        assert!(read_optional_text(&path, 32).is_err());
        assert!(atomic_replace(&path, b"new").is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "data");
    }

    #[test]
    fn oversized_and_invalid_utf8_files_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");

        fs::write(&path, b"12345").unwrap();
        assert!(read_optional_text(&path, 4).is_err());

        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(read_optional_text(&path, 4).is_err());
    }

    #[test]
    fn atomic_replacement_and_removal_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("uid");

        atomic_replace(&path, b"old-id").unwrap();
        atomic_replace(&path, b"new-id").unwrap();
        assert_eq!(load_uid(&path).unwrap().as_deref(), Some("new-id"));

        remove_committed(&path).unwrap();
        remove_committed(&path).unwrap();
        assert!(load_uid(&path).unwrap().is_none());
    }

    #[test]
    fn valid_cross_file_snapshot_does_not_require_connected_hardware() {
        let usb = parse_usb("/dev/ttyUSB0,aabbccddee01,RELAY-A,0\n").unwrap();

        let gen5 = parse_gen5("/dev/ttyUSB2,/dev/ttyUSB3,aabbccddee02\n").unwrap();

        // These paths are validated syntactically. No /dev or sysfs lookup
        // should occur during persisted-state loading.
        validate_snapshot(&usb, &gen5).unwrap();
    }

    #[test]
    fn cross_file_device_collision_is_rejected() {
        let usb = parse_usb("/dev/ttyUSB0,aabbccddee01,RELAY-A,0\n").unwrap();

        // Gen5 attempts to reuse the Gen4 console as its power interface.
        let gen5 = parse_gen5("/dev/ttyUSB2,/dev/ttyUSB0,aabbccddee02\n").unwrap();

        assert!(validate_snapshot(&usb, &gen5).is_err());
    }

    #[test]
    fn persisted_relay_mapping_does_not_require_policy_wiring() {
        let dynamic_channel = parse_usb("/dev/ttyUSB0,aabbccddee01,RELAY-A,1\n").unwrap();

        let dynamic_serial = parse_usb("/dev/ttyUSB0,aabbccddee01,OTHER-RELAY,0\n").unwrap();

        validate_snapshot(&dynamic_channel, &Gen5Mappings::new()).unwrap();
        validate_snapshot(&dynamic_serial, &Gen5Mappings::new()).unwrap();
    }

    #[test]
    fn persisted_relay_mapping_accepts_a_dynamically_discovered_board() {
        let usb = parse_usb("/dev/ttyUSB0,aabbccddee99,RELAY-A,0\n").unwrap();

        validate_snapshot(&usb, &Gen5Mappings::new()).unwrap();
    }

    #[test]
    fn persisted_gen5_mapping_accepts_a_dynamically_discovered_board() {
        let gen5 = parse_gen5("/dev/ttyUSB2,/dev/ttyUSB3,aabbccddee01\n").unwrap();

        validate_snapshot(&UsbMappings::new(), &gen5).unwrap();
    }

    #[test]
    fn malformed_second_file_rejects_the_entire_load_without_rewriting_files() {
        let directory = tempfile::tempdir().unwrap();
        let usb_path = directory.path().join("usb.csv");
        let gen5_path = directory.path().join("gen5.csv");

        let usb_contents = "/dev/ttyUSB0,aabbccddee01,RELAY-A,0\n";
        let bad_gen5_contents = "/dev/ttyUSB2,missing-fields\n";

        fs::write(&usb_path, usb_contents).unwrap();
        fs::write(&gen5_path, bad_gen5_contents).unwrap();

        let result = load_mappings(&usb_path, &gen5_path);

        assert!(result.is_err());

        // Startup validation must not silently repair or discard evidence.
        assert_eq!(fs::read_to_string(&usb_path).unwrap(), usb_contents);
        assert_eq!(fs::read_to_string(&gen5_path).unwrap(), bad_gen5_contents);
    }

    #[test]
    fn missing_mapping_files_mean_uninitialized_state() {
        let directory = tempfile::tempdir().unwrap();

        let (usb, gen5) = load_mappings(
            &directory.path().join("missing-usb.csv"),
            &directory.path().join("missing-gen5.csv"),
        )
        .unwrap();

        assert!(usb.is_empty());
        assert!(gen5.is_empty());
    }

    #[test]
    fn non_regular_state_path_is_not_treated_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let usb_path = directory.path().join("usb.csv");

        fs::create_dir(&usb_path).unwrap();

        assert!(load_mappings(&usb_path, &directory.path().join("missing-gen5.csv"),).is_err());
    }

    #[test]
    fn inconsistent_gen5_key_and_entry_mac_is_rejected() {
        let mut mappings = Gen5Mappings::new();

        mappings.insert(
            "aabbccddee02".into(),
            Gen5MapEntry {
                uart: "/dev/ttyUSB2".into(),
                power: "/dev/ttyUSB3".into(),
                mac: "aabbccddee99".into(),
            },
        );

        assert!(encode_gen5(&mappings).is_err());
    }

    #[test]
    fn uid_file_accepts_only_one_optional_terminal_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("uid");

        for contents in ["controller-123", "controller-123\n", "controller-123\r\n"] {
            fs::write(&path, contents).unwrap();

            assert_eq!(load_uid(&path).unwrap().as_deref(), Some("controller-123"));
        }

        for contents in [
            "",
            "\n",
            "controller-123\n\n",
            " controller-123",
            "controller-123 ",
            "controller-123\r",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(load_uid(&path).is_err());
        }
    }

    #[test]
    fn invalid_write_does_not_replace_existing_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");

        atomic_replace(&path, b"original").unwrap();

        let oversized = vec![b'x'; MAX_STATE_BYTES + 1];
        assert!(atomic_replace(&path, &oversized).is_err());

        assert_eq!(fs::read(&path).unwrap(), b"original");
    }
}
