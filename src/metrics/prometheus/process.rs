//! Process resource usage read from `/proc` at scrape time.
//!
//! Linux exposes these values as small in-memory files, so reading them per
//! scrape costs a few syscalls and needs no background sampler. Other
//! platforms report nothing rather than guessing.

/// Resource usage of the proxy process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessStats {
    /// User plus system CPU time.
    pub cpu_seconds: f64,
    pub resident_memory_bytes: u64,
    pub open_fds: u64,
    pub threads: u64,
}

impl ProcessStats {
    /// Read the current process's usage, or `None` where `/proc` is unavailable.
    #[must_use]
    pub fn read() -> Option<Self> {
        imp::read()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcessStats;

    pub(super) fn read() -> Option<ProcessStats> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let (cpu_ticks, threads) = parse_stat(&stat)?;
        let ticks_per_second = rustix::param::clock_ticks_per_second();
        if ticks_per_second == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // CPU seconds are an approximate gauge input.
        let cpu_seconds = cpu_ticks as f64 / ticks_per_second as f64;
        Some(ProcessStats {
            cpu_seconds,
            resident_memory_bytes: parse_vm_rss_bytes(&status)?,
            open_fds: count_open_fds(),
            threads,
        })
    }

    /// Parse user+system CPU ticks and the thread count from `/proc/self/stat`.
    ///
    /// The command name in field 2 is in parentheses and may itself contain
    /// spaces or parentheses, so fields are counted from the last `)`.
    pub(super) fn parse_stat(stat: &str) -> Option<(u64, u64)> {
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split(' ').filter(|field| !field.is_empty());
        // `rest` starts at field 3 (state); utime is field 14, stime 15,
        // num_threads 20.
        let utime: u64 = fields.nth(11)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        let threads: u64 = fields.nth(4)?.parse().ok()?;
        Some((utime.saturating_add(stime), threads))
    }

    /// Parse `VmRSS` (reported in kB) from `/proc/self/status`.
    pub(super) fn parse_vm_rss_bytes(status: &str) -> Option<u64> {
        status.lines().find_map(|line| {
            let value = line.strip_prefix("VmRSS:")?;
            let kb: u64 = value.trim().trim_end_matches("kB").trim().parse().ok()?;
            Some(kb.saturating_mul(1024))
        })
    }

    fn count_open_fds() -> u64 {
        std::fs::read_dir("/proc/self/fd").map_or(0, |dir| dir.count() as u64)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::ProcessStats;

    pub(super) const fn read() -> Option<ProcessStats> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::imp::{parse_stat, parse_vm_rss_bytes};
    use super::*;

    #[test]
    fn parse_stat_counts_fields_after_a_command_name_with_spaces() {
        let stat = "1234 (nntp (proxy) x) S 1 1234 1234 0 -1 4194560 100 0 0 0 \
                    250 50 0 0 20 0 7 0 12345 1000000 500 18446744073709551615";
        assert_eq!(parse_stat(stat), Some((300, 7)));
    }

    #[test]
    fn parse_vm_rss_reads_kilobytes() {
        let status = "Name:\tnntp-proxy\nVmPeak:\t  9000 kB\nVmRSS:\t  2048 kB\nThreads:\t7\n";
        assert_eq!(parse_vm_rss_bytes(status), Some(2048 * 1024));
    }

    #[test]
    fn reading_this_process_reports_plausible_values() {
        let stats = ProcessStats::read().expect("linux exposes /proc/self");
        assert!(stats.resident_memory_bytes > 0);
        assert!(stats.threads >= 1);
        assert!(stats.open_fds >= 1);
        assert!(stats.cpu_seconds >= 0.0);
    }
}
