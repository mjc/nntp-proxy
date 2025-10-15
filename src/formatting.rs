//! Log formatting utilities

/// Format bytes in human-readable format (KB, MB, GB)
#[inline]
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Shorten UUID to first 8 characters for log readability
#[inline]
pub fn short_id(uuid: &uuid::Uuid) -> String {
    let s = uuid.to_string();
    s[..8].to_owned()
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
        assert_eq!(format_bytes(1048576), "1.00 MB");
        assert_eq!(format_bytes(29312178), "27.95 MB");
        assert_eq!(format_bytes(1073741824), "1.00 GB");
    }

    #[test]
    fn test_short_id() {
        let uuid = uuid::Uuid::parse_str("34925aee-7f65-4670-9adc-d2e95ac97b26").unwrap();
        assert_eq!(short_id(&uuid), "34925aee");
    }
}
