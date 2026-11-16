//! Point-in-time scrape data and its rendering.
//!
//! Gathering and rendering are split so the renderer is a pure function of
//! plain numbers: it can be tested with hostile backend names and without a
//! running proxy, and gathering stays a list of relaxed loads.

use super::process::ProcessStats;
use super::text::{Exposition, MetricKind};
use crate::metrics::histogram::HistogramSnapshot;
use crate::metrics::{BackendCounters, CommandClass, ProxyCounters};
use crate::pool::admission::BackendLoadSnapshot;
use crate::pool::churn::ConnectionChurnSnapshot;

/// Compile-time build identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    pub version: &'static str,
    /// Commit the binary was built from, when the build supplied one through
    /// `NNTP_PROXY_GIT_SHA`.
    pub git_sha: &'static str,
}

impl BuildInfo {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            git_sha: match option_env!("NNTP_PROXY_GIT_SHA") {
                Some(sha) => sha,
                None => "unknown",
            },
        }
    }
}

/// Whether login backoff is holding new connections back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginGate {
    Open,
    BackingOff,
}

/// Pool occupancy at scrape time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolScrape {
    /// Connections that exist, idle or checked out.
    pub size: u64,
    pub idle: u64,
    pub max_size: u64,
    /// Checkouts queued for a connection.
    pub waiting: u64,
    /// Slots withheld after broken connections.
    pub cooldowns: u64,
}

/// Everything exported for one backend.
#[derive(Debug, Clone)]
pub struct BackendScrape {
    /// Configured server name, made unique if two servers share one.
    pub name: String,
    pub tier: String,
    pub counters: BackendCounters,
    pub pool: PoolScrape,
    pub pending_commands: u64,
    pub stateful_sessions: u64,
    pub churn: ConnectionChurnSnapshot,
    pub login_gate: LoginGate,
    pub health_checks: u64,
    pub health_check_failures: u64,
    /// Time from sending a command to its first response bytes.
    pub first_byte: HistogramSnapshot,
    /// Time one command held a connection, first write to end of response.
    pub exchange_duration: HistogramSnapshot,
    /// Backlog admission counters and the service-time average.
    pub admission: BackendLoadSnapshot,
    /// How long a checkout joining the pool queue now would wait.
    pub estimated_wait_seconds: f64,
}

/// Article/availability cache state at scrape time.
pub type CacheScrape = crate::cache::CacheScrapeStats;

/// One complete scrape.
#[derive(Debug, Clone)]
pub struct Scrape {
    pub build: BuildInfo,
    pub process: Option<ProcessStats>,
    pub proxy: ProxyCounters,
    pub cache: CacheScrape,
    pub backends: Vec<BackendScrape>,
}

/// Rough bytes per backend in the output, to size the buffer once.
const BYTES_PER_BACKEND: usize = 3 * 1024;

/// Render a scrape in text exposition format 0.0.4.
#[must_use]
pub fn render(scrape: &Scrape) -> String {
    let mut out = Exposition::with_capacity(4096 + scrape.backends.len() * BYTES_PER_BACKEND);
    render_process(&mut out, scrape);
    render_proxy(&mut out, &scrape.proxy);
    render_cache(&mut out, &scrape.cache);
    render_backends(&mut out, &scrape.backends);
    out.finish()
}

fn render_process(out: &mut Exposition, scrape: &Scrape) {
    out.family(
        "nntp_proxy_build_info",
        MetricKind::Gauge,
        "Build identity of the running proxy; always 1.",
    );
    out.sample(
        "nntp_proxy_build_info",
        &[
            ("version", scrape.build.version),
            ("git_sha", scrape.build.git_sha),
        ],
        1u64,
    );

    let start = scrape
        .proxy
        .start_time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    out.family(
        "nntp_proxy_start_time_seconds",
        MetricKind::Gauge,
        "Unix time the proxy process started.",
    );
    out.sample("nntp_proxy_start_time_seconds", &[], start);

    let Some(process) = scrape.process else {
        return;
    };
    out.family(
        "nntp_proxy_process_cpu_seconds_total",
        MetricKind::Counter,
        "User and system CPU time consumed by the proxy process.",
    );
    out.sample(
        "nntp_proxy_process_cpu_seconds_total",
        &[],
        process.cpu_seconds,
    );
    out.family(
        "nntp_proxy_process_resident_memory_bytes",
        MetricKind::Gauge,
        "Resident set size of the proxy process.",
    );
    out.sample(
        "nntp_proxy_process_resident_memory_bytes",
        &[],
        process.resident_memory_bytes,
    );
    out.family(
        "nntp_proxy_process_open_fds",
        MetricKind::Gauge,
        "File descriptors open in the proxy process, sockets included.",
    );
    out.sample("nntp_proxy_process_open_fds", &[], process.open_fds);
    out.family(
        "nntp_proxy_process_threads",
        MetricKind::Gauge,
        "Operating system threads in the proxy process.",
    );
    out.sample("nntp_proxy_process_threads", &[], process.threads);
}

fn render_proxy(out: &mut Exposition, proxy: &ProxyCounters) {
    out.family(
        "nntp_proxy_client_sessions_active",
        MetricKind::Gauge,
        "Client connections currently open.",
    );
    out.sample(
        "nntp_proxy_client_sessions_active",
        &[],
        proxy.client_sessions_active,
    );
    out.family(
        "nntp_proxy_client_sessions_total",
        MetricKind::Counter,
        "Client connections accepted. Restored from the stats file at startup, then only grows.",
    );
    out.sample(
        "nntp_proxy_client_sessions_total",
        &[],
        proxy.client_sessions_total,
    );
    out.family(
        "nntp_proxy_stateful_sessions_active",
        MetricKind::Gauge,
        "Client sessions currently pinned to one backend connection.",
    );
    out.sample(
        "nntp_proxy_stateful_sessions_active",
        &[],
        proxy.stateful_sessions_active,
    );

    out.family(
        "nntp_proxy_client_commands_total",
        MetricKind::Counter,
        "Commands read from clients, by command class.",
    );
    for (class, count) in CommandClass::ALL.iter().zip(proxy.client_commands) {
        out.sample(
            "nntp_proxy_client_commands_total",
            &[("command", class.label())],
            count,
        );
    }

    out.family(
        "nntp_proxy_client_bytes_total",
        MetricKind::Counter,
        "Bytes exchanged with clients, added when each client session ends. Use nntp_proxy_backend_bytes_total for live throughput.",
    );
    out.sample(
        "nntp_proxy_client_bytes_total",
        &[("direction", "received")],
        proxy.client_bytes_received,
    );
    out.sample(
        "nntp_proxy_client_bytes_total",
        &[("direction", "sent")],
        proxy.client_bytes_sent,
    );

    out.family(
        "nntp_proxy_pipeline_batches_total",
        MetricKind::Counter,
        "Pipelined client command batches of more than one command.",
    );
    out.sample(
        "nntp_proxy_pipeline_batches_total",
        &[],
        proxy.pipeline_batches,
    );
    out.family(
        "nntp_proxy_pipeline_commands_total",
        MetricKind::Counter,
        "Commands carried in pipelined batches.",
    );
    out.sample(
        "nntp_proxy_pipeline_commands_total",
        &[],
        proxy.pipeline_commands,
    );
}

fn render_cache(out: &mut Exposition, cache: &CacheScrape) {
    if let Some(entries) = cache.entries {
        out.family(
            "nntp_proxy_cache_entries",
            MetricKind::Gauge,
            "Entries in the article/availability cache.",
        );
        out.sample("nntp_proxy_cache_entries", &[], entries);
    }
    if let Some(size_bytes) = cache.size_bytes {
        out.family(
            "nntp_proxy_cache_size_bytes",
            MetricKind::Gauge,
            "Memory used by the article cache.",
        );
        out.sample("nntp_proxy_cache_size_bytes", &[], size_bytes);
    }
    out.family(
        "nntp_proxy_cache_capacity_bytes",
        MetricKind::Gauge,
        "Configured memory capacity of the article/availability cache.",
    );
    out.sample("nntp_proxy_cache_capacity_bytes", &[], cache.capacity_bytes);
    out.family(
        "nntp_proxy_cache_lookups_total",
        MetricKind::Counter,
        "Cache lookups by result.",
    );
    out.sample(
        "nntp_proxy_cache_lookups_total",
        &[("result", "hit")],
        cache.hits,
    );
    out.sample(
        "nntp_proxy_cache_lookups_total",
        &[("result", "miss")],
        cache.misses,
    );

    let Some(disk) = &cache.disk else {
        return;
    };
    out.family(
        "nntp_proxy_cache_disk_hits_total",
        MetricKind::Counter,
        "Cache hits served from the disk tier.",
    );
    out.sample("nntp_proxy_cache_disk_hits_total", &[], disk.disk_hits);
    out.family(
        "nntp_proxy_cache_disk_capacity_bytes",
        MetricKind::Gauge,
        "Configured disk-tier capacity.",
    );
    out.sample("nntp_proxy_cache_disk_capacity_bytes", &[], disk.capacity);
    out.family(
        "nntp_proxy_cache_disk_bytes_total",
        MetricKind::Counter,
        "Bytes moved to and from the disk tier.",
    );
    out.sample(
        "nntp_proxy_cache_disk_bytes_total",
        &[("direction", "read")],
        disk.bytes_read,
    );
    out.sample(
        "nntp_proxy_cache_disk_bytes_total",
        &[("direction", "written")],
        disk.bytes_written,
    );
    out.family(
        "nntp_proxy_cache_disk_ios_total",
        MetricKind::Counter,
        "Disk-tier I/O operations.",
    );
    out.sample(
        "nntp_proxy_cache_disk_ios_total",
        &[("direction", "read")],
        disk.read_ios,
    );
    out.sample(
        "nntp_proxy_cache_disk_ios_total",
        &[("direction", "written")],
        disk.write_ios,
    );
}

/// Emit one per-backend family whose samples differ only by backend.
fn backend_family(
    out: &mut Exposition,
    backends: &[BackendScrape],
    family: (&str, MetricKind, &str),
    value: impl Fn(&BackendScrape) -> u64,
) {
    let (name, kind, help) = family;
    out.family(name, kind, help);
    for backend in backends {
        out.sample(
            name,
            &[("backend", &backend.name), ("tier", &backend.tier)],
            value(backend),
        );
    }
}

/// Reads one value out of a backend's scrape.
type BackendValue = fn(&BackendScrape) -> u64;

/// Emit one per-backend family with an extra label taking each listed value.
fn backend_family_by(
    out: &mut Exposition,
    backends: &[BackendScrape],
    family: (&str, MetricKind, &str),
    label: &str,
    variants: &[(&str, BackendValue)],
) {
    let (name, kind, help) = family;
    out.family(name, kind, help);
    for backend in backends {
        for (variant, value) in variants {
            out.sample(
                name,
                &[
                    ("backend", &backend.name),
                    ("tier", &backend.tier),
                    (label, variant),
                ],
                value(backend),
            );
        }
    }
}

/// Emit one per-backend histogram family.
fn backend_histogram(
    out: &mut Exposition,
    backends: &[BackendScrape],
    name: &str,
    help: &str,
    histogram: fn(&BackendScrape) -> &HistogramSnapshot,
) {
    let bucket = format!("{name}_bucket");
    let sum = format!("{name}_sum");
    let count = format!("{name}_count");
    out.family(name, MetricKind::Histogram, help);
    for backend in backends {
        let snapshot = histogram(backend);
        let labels = [
            ("backend", backend.name.as_str()),
            ("tier", backend.tier.as_str()),
        ];
        let mut cumulative = 0;
        for (le, in_bucket) in snapshot.bounds.labels.iter().zip(snapshot.counts) {
            cumulative += in_bucket;
            out.sample(&bucket, &[labels[0], labels[1], ("le", le)], cumulative);
        }
        let total = snapshot.count();
        out.sample(&bucket, &[labels[0], labels[1], ("le", "+Inf")], total);
        #[allow(clippy::cast_precision_loss)] // Seconds sums are approximate by nature.
        let sum_seconds = snapshot.sum_micros as f64 / 1_000_000.0;
        out.sample(&sum, &labels, sum_seconds);
        out.sample(&count, &labels, total);
    }
}

/// Backlog admission and failover counters: why requests skipped, waited
/// on, or gave up on a backend without it returning `430`.
fn render_admission(out: &mut Exposition, backends: &[BackendScrape]) {
    use MetricKind::{Counter, Gauge};

    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_backlog_skips_total",
            Counter,
            "Checkouts that skipped the backend because its connection queue would not serve them in time.",
        ),
        |b| b.admission.backlog_skips,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_busy_timeouts_total",
            Counter,
            "Bounded checkouts that waited their whole limit without getting a connection.",
        ),
        |b| b.admission.busy_timeouts,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_deadline_expiries_total",
            Counter,
            "Client requests whose deadline ran out with this backend next in line.",
        ),
        |b| b.admission.deadline_expiries,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_last_resort_waits_total",
            Counter,
            "Checkouts that queued on the backend because every other backend left for the request was backlogged.",
        ),
        |b| b.admission.last_resort_waits,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_last_resort_connected_total",
            Counter,
            "Last-resort checkouts that got a connection.",
        ),
        |b| b.admission.last_resort_connected,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_last_resort_timeouts_total",
            Counter,
            "Last-resort checkouts that waited their whole limit without a connection.",
        ),
        |b| b.admission.last_resort_timeouts,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_error_reply_failovers_total",
            Counter,
            "Non-430 error replies to article lookups that were retried on another backend instead of sent to the client.",
        ),
        |b| b.counters.error_reply_failovers,
    );

    out.family(
        "nntp_proxy_backend_service_time_seconds",
        Gauge,
        "Moving average of how long one command holds a connection, used for backlog admission; 0 before the first sample.",
    );
    for backend in backends {
        #[allow(clippy::cast_precision_loss)] // A smoothed average; exact micros are not needed.
        let seconds = backend.admission.service_time_ewma_micros as f64 / 1_000_000.0;
        out.sample(
            "nntp_proxy_backend_service_time_seconds",
            &[("backend", &backend.name), ("tier", &backend.tier)],
            seconds,
        );
    }
    out.family(
        "nntp_proxy_backend_estimated_wait_seconds",
        Gauge,
        "Estimated wait for a connection if a checkout joined the backend's queue now.",
    );
    for backend in backends {
        out.sample(
            "nntp_proxy_backend_estimated_wait_seconds",
            &[("backend", &backend.name), ("tier", &backend.tier)],
            backend.estimated_wait_seconds,
        );
    }
    out.family(
        "nntp_proxy_backend_article_standing",
        Gauge,
        "Recent article-lookup standing from 0 to 1: falls by 1/8 on each 430 or refused lookup and climbs by 1/128 of the gap on each hit. With routing.article.standing = \"adaptive\" it scales the backend's share of article lookups within its tier.",
    );
    for backend in backends {
        let standing = f64::from(backend.admission.standing_ppm)
            / f64::from(crate::pool::admission::STANDING_FULL_PPM);
        out.sample(
            "nntp_proxy_backend_article_standing",
            &[("backend", &backend.name), ("tier", &backend.tier)],
            standing,
        );
    }
}

fn render_backends(out: &mut Exposition, backends: &[BackendScrape]) {
    use MetricKind::{Counter, Gauge};

    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_commands_total",
            Counter,
            "Commands sent to the backend.",
        ),
        |b| b.counters.commands,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_articles_total",
            Counter,
            "Successful ARTICLE/BODY/HEAD responses from the backend.",
        ),
        |b| b.counters.articles,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_article_bytes_total",
            Counter,
            "Bytes of successful ARTICLE/BODY/HEAD responses from the backend.",
        ),
        |b| b.counters.article_bytes,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_article_missing_total",
            Counter,
            "Authoritative 430 (no such article) replies from the backend.",
        ),
        |b| b.counters.article_missing,
    );
    backend_family_by(
        out,
        backends,
        (
            "nntp_proxy_backend_error_replies_total",
            Counter,
            "Error status replies from the backend by class. The 4xx class includes 430 replies, which are also in nntp_proxy_backend_article_missing_total.",
        ),
        "code_class",
        &[
            ("4xx", |b| b.counters.errors_4xx),
            ("5xx", |b| b.counters.errors_5xx),
        ],
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_errors_total",
            Counter,
            "All errors attributed to the backend: 4xx and 5xx replies plus transport and session failures.",
        ),
        |b| b.counters.errors,
    );
    backend_family_by(
        out,
        backends,
        (
            "nntp_proxy_backend_bytes_total",
            Counter,
            "Bytes exchanged with the backend. Stateful sessions add their bytes when the session ends.",
        ),
        "direction",
        &[
            ("sent", |b| b.counters.bytes_to_backend),
            ("received", |b| b.counters.bytes_from_backend),
        ],
    );

    backend_family_by(
        out,
        backends,
        (
            "nntp_proxy_backend_pool_connections",
            Gauge,
            "Pooled backend connections by state.",
        ),
        "state",
        &[
            ("idle", |b| b.pool.idle),
            ("in_use", |b| b.pool.size.saturating_sub(b.pool.idle)),
        ],
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_pool_max_connections",
            Gauge,
            "Current pool size limit, lowered temporarily while cooldowns are active.",
        ),
        |b| b.pool.max_size,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_pool_waiters",
            Gauge,
            "Checkouts waiting for a pooled connection.",
        ),
        |b| b.pool.waiting,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_pool_cooldowns",
            Gauge,
            "Pool slots withheld after broken connections.",
        ),
        |b| b.pool.cooldowns,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_pending_commands",
            Gauge,
            "Commands routed to the backend and not yet finished.",
        ),
        |b| b.pending_commands,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_stateful_sessions",
            Gauge,
            "Client sessions pinned to the backend.",
        ),
        |b| b.stateful_sessions,
    );

    backend_family_by(
        out,
        backends,
        (
            "nntp_proxy_backend_connect_attempts_total",
            Counter,
            "Attempts to open a backend connection by outcome.",
        ),
        "outcome",
        &[
            ("created", |b| b.churn.created),
            ("failed", |b| b.churn.failed),
            ("login_refused", |b| b.churn.login_refused),
            ("backoff_denied", |b| b.churn.backoff_denied),
        ],
    );
    backend_family_by(
        out,
        backends,
        (
            "nntp_proxy_backend_connections_closed_total",
            Counter,
            "Backend connections closed, by reason. Created minus all closes is the live connection count.",
        ),
        "reason",
        &[
            ("backend_error", |b| b.churn.closed_backend_error),
            ("cancelled", |b| b.churn.closed_cancelled),
            ("client_abort", |b| b.churn.closed_client_abort),
            ("health_check", |b| b.churn.closed_health_check),
            ("stale", |b| b.churn.closed_stale),
            ("idle", |b| b.churn.closed_idle),
        ],
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_login_backoff",
            Gauge,
            "1 while login backoff makes new connections fail locally (window open, or its single probe login in flight).",
        ),
        |b| match b.login_gate {
            LoginGate::Open => 0,
            LoginGate::BackingOff => 1,
        },
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_health_checks_total",
            Counter,
            "Idle connections checked by the periodic health check.",
        ),
        |b| b.health_checks,
    );
    render_admission(out, backends);
    backend_histogram(
        out,
        backends,
        "nntp_proxy_backend_first_byte_seconds",
        "Time from sending a command to the backend until its first response bytes arrive.",
        |b| &b.first_byte,
    );
    backend_histogram(
        out,
        backends,
        "nntp_proxy_backend_exchange_duration_seconds",
        "Time one command held a backend connection, from its first write to the end of the response. For article fetches this is the whole transfer.",
        |b| &b.exchange_duration,
    );
    backend_family(
        out,
        backends,
        (
            "nntp_proxy_backend_health_check_failures_total",
            Counter,
            "Idle connections that failed the periodic health check.",
        ),
        |b| b.health_check_failures,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::metrics::histogram::{EXCHANGE_BUCKETS, FIRST_BYTE_BUCKETS, LatencyHistogram};

    pub(crate) fn sample_scrape(names: &[&str]) -> Scrape {
        Scrape {
            build: BuildInfo {
                version: "0.0.0-test",
                git_sha: "unknown",
            },
            process: Some(ProcessStats {
                cpu_seconds: 1.5,
                resident_memory_bytes: 4096,
                open_fds: 9,
                threads: 3,
            }),
            proxy: ProxyCounters {
                client_sessions_total: 5,
                client_sessions_active: 2,
                stateful_sessions_active: 1,
                client_commands: [1; CommandClass::COUNT],
                client_bytes_received: 10,
                client_bytes_sent: 20,
                pipeline_batches: 0,
                pipeline_commands: 0,
                start_time: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            },
            cache: CacheScrape {
                entries: Some(1),
                size_bytes: Some(2),
                capacity_bytes: 3,
                hits: 4,
                misses: 5,
                disk: Some(crate::cache::DiskDisplayStats::default()),
            },
            backends: names
                .iter()
                .enumerate()
                .map(|(i, name)| BackendScrape {
                    name: (*name).to_string(),
                    tier: i.to_string(),
                    counters: BackendCounters {
                        commands: 10,
                        article_missing: 3,
                        errors_4xx: 4,
                        ..BackendCounters::default()
                    },
                    pool: PoolScrape {
                        size: 4,
                        idle: 1,
                        max_size: 8,
                        waiting: 0,
                        cooldowns: 0,
                    },
                    pending_commands: 0,
                    stateful_sessions: 0,
                    churn: ConnectionChurnSnapshot::default(),
                    login_gate: LoginGate::BackingOff,
                    health_checks: 0,
                    health_check_failures: 0,
                    first_byte: {
                        let h = LatencyHistogram::new(&FIRST_BYTE_BUCKETS);
                        h.observe_micros(3_000);
                        h.observe_micros(3_000);
                        h.observe_micros(20_000_000);
                        h.snapshot()
                    },
                    exchange_duration: HistogramSnapshot::empty(&EXCHANGE_BUCKETS),
                    admission: BackendLoadSnapshot {
                        backlog_skips: 7,
                        service_time_ewma_micros: 250_000,
                        standing_ppm: 250_000,
                        ..BackendLoadSnapshot::default()
                    },
                    estimated_wait_seconds: 1.25,
                })
                .collect(),
        }
    }

    /// A parsed sample line: metric name, label pairs (unescaped), value.
    #[derive(Debug)]
    pub(crate) struct ParsedSample {
        pub name: String,
        pub labels: Vec<(String, String)>,
        pub value: f64,
    }

    fn is_name_char(c: char, first: bool) -> bool {
        c.is_ascii_alphabetic() || c == '_' || c == ':' || (!first && c.is_ascii_digit())
    }

    /// Strict parser for one sample line of the text format; returns `None`
    /// for anything a Prometheus scraper would reject.
    pub(crate) fn parse_sample_line(line: &str) -> Option<ParsedSample> {
        let mut chars = line.char_indices().peekable();
        let mut name = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if is_name_char(c, name.is_empty()) {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            return None;
        }
        let mut labels = Vec::new();
        if chars.peek().map(|&(_, c)| c) == Some('{') {
            chars.next();
            loop {
                if chars.peek().map(|&(_, c)| c) == Some('}') {
                    chars.next();
                    break;
                }
                let mut key = String::new();
                while let Some(&(_, c)) = chars.peek() {
                    if is_name_char(c, key.is_empty()) {
                        key.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if key.is_empty() || chars.next()?.1 != '=' || chars.next()?.1 != '"' {
                    return None;
                }
                let mut value = String::new();
                loop {
                    match chars.next()?.1 {
                        '"' => break,
                        '\\' => match chars.next()?.1 {
                            '\\' => value.push('\\'),
                            '"' => value.push('"'),
                            'n' => value.push('\n'),
                            _ => return None,
                        },
                        '\n' => return None,
                        c => value.push(c),
                    }
                }
                labels.push((key, value));
                match chars.next()?.1 {
                    ',' => {}
                    '}' => break,
                    _ => return None,
                }
            }
        }
        if chars.next()?.1 != ' ' {
            return None;
        }
        let rest: String = chars.map(|(_, c)| c).collect();
        let value = match rest.as_str() {
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            "NaN" => f64::NAN,
            other => other.parse().ok()?,
        };
        Some(ParsedSample {
            name,
            labels,
            value,
        })
    }

    /// Check the document's structure and return its samples.
    ///
    /// Every sample must belong to the family most recently opened by a HELP
    /// and TYPE pair, each family must be opened once, and counter families
    /// must end in `_total`.
    pub(crate) fn validate_exposition(text: &str) -> Vec<ParsedSample> {
        use std::collections::HashSet;
        let mut seen_families = HashSet::new();
        let mut seen_series = HashSet::new();
        let mut current: Option<(String, String)> = None;
        let mut pending_help: Option<String> = None;
        let mut samples = Vec::new();
        assert!(
            text.as_bytes().last() == Some(&b'\n'),
            "document ends with newline"
        );
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, _) = rest.split_once(' ').expect("HELP has text");
                assert!(pending_help.is_none(), "HELP without TYPE before {name}");
                assert!(
                    seen_families.insert(name.to_string()),
                    "family {name} repeated"
                );
                pending_help = Some(name.to_string());
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE has kind");
                assert_eq!(
                    pending_help.take().as_deref(),
                    Some(name),
                    "TYPE follows HELP"
                );
                assert!(matches!(kind, "counter" | "gauge" | "histogram"), "{kind}");
                if kind == "counter" {
                    assert!(
                        name.strip_suffix("_total").is_some(),
                        "counter {name} ends in _total"
                    );
                }
                assert!(
                    name.strip_prefix("nntp_proxy_").is_some(),
                    "{name} has the nntp_proxy_ prefix"
                );
                current = Some((name.to_string(), kind.to_string()));
            } else {
                let sample = parse_sample_line(line)
                    .unwrap_or_else(|| panic!("unparsable sample line: {line:?}"));
                let (family, kind) = current.as_ref().expect("sample after a family header");
                let belongs = if kind == "histogram" {
                    ["_bucket", "_sum", "_count"]
                        .iter()
                        .any(|suffix| sample.name == format!("{family}{suffix}"))
                } else {
                    &sample.name == family
                };
                assert!(belongs, "sample {} outside family {family}", sample.name);
                let series = format!("{}{:?}", sample.name, sample.labels);
                assert!(
                    seen_series.insert(series.clone()),
                    "duplicate series {series}"
                );
                samples.push(sample);
            }
        }
        assert!(pending_help.is_none(), "trailing HELP without TYPE");
        samples
    }

    #[test]
    fn rendered_document_is_valid_exposition() {
        let text = render(&sample_scrape(&["backend-a", "backend-b"]));
        let samples = validate_exposition(&text);
        assert!(samples.len() > 40);
    }

    #[test]
    fn hostile_backend_names_round_trip_through_label_escaping() {
        let hostile = "news \"primary\" \\ east\nline2";
        let text = render(&sample_scrape(&[hostile]));
        let samples = validate_exposition(&text);
        let commands = samples
            .iter()
            .find(|s| s.name == "nntp_proxy_backend_commands_total")
            .expect("commands family present");
        assert_eq!(
            commands.labels[0],
            ("backend".to_string(), hostile.to_string())
        );
        assert_eq!(commands.labels[1], ("tier".to_string(), "0".to_string()));
    }

    #[test]
    fn pool_in_use_is_size_minus_idle_and_backoff_is_one() {
        let text = render(&sample_scrape(&["backend-a"]));
        let samples = validate_exposition(&text);
        let value = |name: &str, extra: Option<(&str, &str)>| {
            samples
                .iter()
                .find(|s| {
                    s.name == name
                        && extra
                            .is_none_or(|(k, v)| s.labels.iter().any(|(lk, lv)| lk == k && lv == v))
                })
                .map(|s| s.value)
        };
        assert_eq!(
            value(
                "nntp_proxy_backend_pool_connections",
                Some(("state", "in_use"))
            ),
            Some(3.0)
        );
        assert_eq!(value("nntp_proxy_backend_login_backoff", None), Some(1.0));
        assert_eq!(
            value("nntp_proxy_backend_article_missing_total", None),
            Some(3.0)
        );
        assert_eq!(
            value("nntp_proxy_backend_backlog_skips_total", None),
            Some(7.0)
        );
        assert_eq!(
            value("nntp_proxy_backend_service_time_seconds", None),
            Some(0.25)
        );
        assert_eq!(
            value("nntp_proxy_backend_article_standing", None),
            Some(0.25)
        );
        assert_eq!(
            value("nntp_proxy_backend_estimated_wait_seconds", None),
            Some(1.25)
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_with_count() {
        let text = render(&sample_scrape(&["backend-a"]));
        let samples = validate_exposition(&text);
        let buckets: Vec<(String, f64)> = samples
            .iter()
            .filter(|s| s.name == "nntp_proxy_backend_first_byte_seconds_bucket")
            .map(|s| {
                let le = s.labels.iter().find(|(k, _)| k == "le").unwrap().1.clone();
                (le, s.value)
            })
            .collect();
        assert_eq!(buckets.len(), crate::metrics::histogram::BUCKETS + 1);
        // 3 ms lands at le=0.005; nothing more until the 20 s outlier.
        assert_eq!(buckets[0], ("0.001".to_string(), 0.0));
        assert_eq!(buckets[2], ("0.005".to_string(), 2.0));
        assert_eq!(buckets[12], ("10".to_string(), 2.0));
        assert_eq!(buckets[13], ("+Inf".to_string(), 3.0));
        assert!(
            buckets
                .iter()
                .zip(buckets.iter().skip(1))
                .all(|(a, b)| a.1 <= b.1)
        );
        let find = |name: &str| samples.iter().find(|s| s.name == name).unwrap().value;
        assert!((find("nntp_proxy_backend_first_byte_seconds_sum") - 20.006).abs() < 1e-9);
        assert_eq!(find("nntp_proxy_backend_first_byte_seconds_count"), 3.0);
        assert_eq!(
            find("nntp_proxy_backend_exchange_duration_seconds_count"),
            0.0
        );
    }

    #[test]
    fn process_and_disk_families_are_omitted_when_unavailable() {
        let mut scrape = sample_scrape(&["backend-a"]);
        scrape.process = None;
        scrape.cache.disk = None;
        scrape.cache.entries = None;
        scrape.cache.size_bytes = None;
        let text = render(&scrape);
        validate_exposition(&text);
        assert!(!text.contains("nntp_proxy_process_"));
        assert!(!text.contains("nntp_proxy_cache_disk_"));
        assert!(!text.contains("nntp_proxy_cache_entries"));
        assert!(!text.contains("nntp_proxy_cache_size_bytes"));
    }

    #[test]
    fn no_backends_still_renders_valid_headers() {
        let text = render(&sample_scrape(&[]));
        validate_exposition(&text);
    }

    #[test]
    fn parser_rejects_malformed_lines() {
        assert!(parse_sample_line("m{a=\"x} 1").is_none());
        assert!(parse_sample_line("m{a=\"\\q\"} 1").is_none());
        assert!(parse_sample_line("m  1").is_none());
        assert!(parse_sample_line("1m 1").is_none());
        assert!(parse_sample_line("m one").is_none());
        assert!(parse_sample_line("m{a=\"x\"} 1").is_some());
    }
}
