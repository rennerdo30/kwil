//! Human byte sizes for settings stored in bytes: one display rule and one
//! parser, so "64 MB" in a field round-trips to the same stored integer.
//! Units are binary (1 KB = 1024 bytes), as the settings ranges are.

const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];

/// The largest unit that states `bytes` exactly, so the text parses back to the
/// same value: 67108864 → "64 MB", 1536 → "1.5 KB" is never produced ("1536 B").
pub fn format_size(bytes: u64) -> String {
    for (unit, name) in UNITS {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {name}", bytes / unit);
        }
    }
    format!("{bytes} B")
}

const HINT: &str = "enter a size such as 64 MB, 512 KB or 1048576";

/// Accepts "64 MB", "64MB", "65536 KB", "1.5 GB" and plain bytes; units are
/// case-insensitive (B, KB, MB, GB, also K/M/G and KiB/MiB/GiB). A fraction
/// rounds to the nearest byte.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let unit: u128 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(HINT.into()),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() || fraction.contains('.') || fraction.len() > 9 || whole.len() > 20 {
        return Err(HINT.into());
    }
    let whole: u128 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| HINT)? };
    let scale = 10u128.pow(fraction.len() as u32);
    let fraction: u128 = if fraction.is_empty() { 0 } else { fraction.parse().map_err(|_| HINT)? };
    let bytes = whole * unit + (fraction * unit * 2 + scale) / (2 * scale);
    u64::try_from(bytes).map_err(|_| HINT.into())
}

/// `parse_size` limited to `min..=max`, with the range in the error text.
pub fn parse_size_within(text: &str, min: u64, max: u64) -> Result<u64, String> {
    let range = || format!("enter a size from {} to {}", format_size(min), format_size(max));
    let bytes = parse_size(text).map_err(|_| range())?;
    if (min..=max).contains(&bytes) { Ok(bytes) } else { Err(range()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_format_in_the_largest_exact_unit_and_parse_back() {
        for (bytes, text) in [
            (0, "0 B"),
            (1, "1 B"),
            (1000, "1000 B"),
            (1024, "1 KB"),
            (1536, "1536 B"),
            (65 * 1024, "65 KB"),
            (64 << 20, "64 MB"),
            (20 << 20, "20 MB"),
            (512 << 20, "512 MB"),
            (3 << 30, "3 GB"),
            ((1 << 30) + (1 << 20), "1025 MB"),
        ] {
            assert_eq!(format_size(bytes), text, "{bytes}");
            assert_eq!(parse_size(text), Ok(bytes), "{text} round-trips");
        }
    }

    #[test]
    fn sizes_accept_the_common_spellings() {
        for text in ["64 MB", "64MB", "64mb", " 64 MiB ", "64M", "65536 KB", "65536k", "67108864", "67108864 bytes"] {
            assert_eq!(parse_size(text), Ok(64 << 20), "{text}");
        }
        assert_eq!(parse_size("1.5 KB"), Ok(1536));
        assert_eq!(parse_size(".5 GB"), Ok(512 << 20));
        assert_eq!(parse_size("0.0001 KB"), Ok(0), "fractions round to the nearest byte");
        assert_eq!(parse_size("0.001 KB"), Ok(1));
    }

    #[test]
    fn sizes_reject_bad_input_with_a_hint() {
        for text in ["", " ", "MB", ".", "-1", "-1 MB", "64 TB", "64 MBs", "1,5 MB", "1.2.3", "1e6", "64 M B", "ten"] {
            let error = parse_size(text).expect_err(text);
            assert!(error.contains("64 MB"), "{text}: {error}");
        }
        assert!(parse_size("18446744073709551616").is_err(), "u64 overflow");
        assert!(parse_size("17179869184 GB").is_err(), "unit overflow");
        assert_eq!(parse_size("18446744073709551615"), Ok(u64::MAX));
    }

    #[test]
    fn sizes_are_checked_against_the_range_with_its_bounds_in_the_error() {
        assert_eq!(parse_size_within("64 MB", 0, 64 << 20), Ok(64 << 20));
        assert_eq!(parse_size_within("0", 0, 64 << 20), Ok(0));
        let above = parse_size_within("67108865", 0, 64 << 20).unwrap_err();
        assert_eq!(above, "enter a size from 0 B to 64 MB");
        assert_eq!(parse_size_within("0", 1, 512 << 20).unwrap_err(), "enter a size from 1 B to 512 MB");
        assert_eq!(parse_size_within("1 B", 1, 512 << 20), Ok(1));
        assert_eq!(parse_size_within("lots", 0, 20 << 20).unwrap_err(), "enter a size from 0 B to 20 MB");
    }

    /// Every byte setting uses the size kind, states its range in sizes, and
    /// that range is exactly the one settings validation enforces.
    #[test]
    fn every_byte_setting_is_a_size_with_the_validated_range() {
        use crate::settings_ui::{SCHEMA, SettingKind, SettingValue, SettingsDocument};
        let mut sizes = 0;
        for spec in SCHEMA.iter().filter(|s| s.id.ends_with("_bytes")) {
            let SettingKind::Bytes { min, max } = spec.kind else { panic!("{} is not a size", spec.id) };
            sizes += 1;
            let range = format!("from {} to {}", format_size(min), format_size(max));
            assert!(spec.description.contains(&range), "{}: {}", spec.id, spec.description);
            let mut doc = SettingsDocument::parse("").unwrap();
            let edit = |doc: &mut SettingsDocument, bytes: u64| doc.edit(spec.id, SettingValue::Integer(bytes as i64));
            edit(&mut doc, max).unwrap();
            edit(&mut doc, min).unwrap();
            assert!(edit(&mut doc, max + 1).is_err(), "{} accepts above {max}", spec.id);
            if min > 0 {
                assert!(edit(&mut doc, min - 1).is_err(), "{} accepts below {min}", spec.id);
            }
            assert_eq!(spec.kind.size_text(&SettingValue::Integer(max as i64)), Some(format_size(max)));
        }
        assert_eq!(sizes, 3, "small file threshold, undo memory, full parse limit");
    }
}
