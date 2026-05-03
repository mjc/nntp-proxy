//! Log formatting utilities

#[allow(clippy::cast_precision_loss)]
fn bytes_as_f64_for_display(bytes: u64) -> f64 {
    // Human-readable log output is approximate by design; exact byte counts are
    // preserved in the integer path below and in metrics storage.
    bytes as f64
}

/// Format bytes in human-readable format (KB, MB, GB)
#[inline]
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const KB_F64: f64 = 1024.0;
    const MB_F64: f64 = KB_F64 * 1024.0;
    const GB_F64: f64 = MB_F64 * 1024.0;

    if bytes >= GB {
        format!("{:.2} GB", bytes_as_f64_for_display(bytes) / GB_F64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes_as_f64_for_display(bytes) / MB_F64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes_as_f64_for_display(bytes) / KB_F64)
    } else {
        format!("{bytes} B")
    }
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
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1_048_576), "1.00 MB");
        assert_eq!(format_bytes(29_312_178), "27.95 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GB");
    }

    #[test]
    fn test_short_id() {
        let uuid = uuid::Uuid::parse_str("34925aee-7f65-4670-9adc-d2e95ac97b26").unwrap();
        assert_eq!(short_id(&uuid), "34925aee");
    }
}
