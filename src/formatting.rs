//! Log formatting utilities

#[allow(clippy::cast_precision_loss)] // Human-readable byte formatting is approximate by design.
const fn bytes_as_f64_for_display(bytes: u64) -> f64 {
    // Human-readable log output is approximate by design; exact byte counts are
    // preserved in the integer path below and in metrics storage.
    bytes as f64
}

/// Format bytes in human-readable format using IEC units (KiB, MiB, GiB).
#[inline]
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    const PIB: u64 = TIB * 1024;
    const KIB_F64: f64 = 1024.0;
    const MIB_F64: f64 = KIB_F64 * 1024.0;
    const GIB_F64: f64 = MIB_F64 * 1024.0;
    const TIB_F64: f64 = GIB_F64 * 1024.0;
    const PIB_F64: f64 = TIB_F64 * 1024.0;

    if bytes >= PIB {
        format!("{:.2} PiB", bytes_as_f64_for_display(bytes) / PIB_F64)
    } else if bytes >= TIB {
        format!("{:.2} TiB", bytes_as_f64_for_display(bytes) / TIB_F64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes_as_f64_for_display(bytes) / GIB_F64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes_as_f64_for_display(bytes) / MIB_F64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes_as_f64_for_display(bytes) / KIB_F64)
    } else {
        format!("{bytes} B")
    }
}

/// Format large counts with compact human suffixes for readable TUI output.
#[inline]
#[must_use]
pub fn format_count(count: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
    ];

    #[allow(clippy::cast_precision_loss)] // Presentation-only rounding for compact labels.
    fn compact(value: u64, divisor: u64, suffix: &str) -> String {
        let scaled = value as f64 / divisor as f64;
        format!("{scaled:.1}{suffix}")
    }

    for (idx, (divisor, suffix)) in UNITS.iter().enumerate() {
        if count >= *divisor {
            let formatted = compact(count, *divisor, suffix);
            if formatted.starts_with("1000.0") && idx > 0 {
                let (next_divisor, next_suffix) = UNITS[idx - 1];
                return compact(count, next_divisor, next_suffix);
            }
            return formatted;
        }
    }

    count.to_string()
}

/// Shorten UUID to first 8 characters for log readability
#[inline]
#[must_use]
pub fn short_id(uuid: &uuid::Uuid) -> String {
    uuid.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(1_048_576), "1.00 MiB");
        assert_eq!(format_bytes(29_312_178), "27.95 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.0K");
        assert_eq!(format_count(999_999), "1.0M");
        assert_eq!(format_count(12_345), "12.3K");
        assert_eq!(format_count(1_234_567), "1.2M");
        assert_eq!(format_count(1_234_567_890), "1.2B");
        assert_eq!(format_count(9_876_543_210_987), "9.9T");
    }

    #[test]
    fn test_short_id() {
        let uuid = uuid::Uuid::parse_str("34925aee-7f65-4670-9adc-d2e95ac97b26").unwrap();
        assert_eq!(short_id(&uuid), "34925aee");
    }
}
