//! Contention cost of the per-command client counter.
//!
//! Every client command adds to the command-class counter from whichever
//! worker thread handles it, so the interesting number is the cost per add
//! with several threads adding at once.
//!
//! Run with: cargo bench --bench `client_command_counters`

use nntp_proxy::metrics::MetricsCollector;
use nntp_proxy::protocol::RequestKind;
use std::sync::LazyLock;

fn main() {
    divan::main();
}

static COLLECTOR: LazyLock<MetricsCollector> = LazyLock::new(|| MetricsCollector::new(1));

#[divan::bench(threads = [1, 2, 4, 8], sample_count = 200, sample_size = 10_000)]
fn record_client_command() {
    COLLECTOR.record_client_command(divan::black_box(RequestKind::Body));
}
