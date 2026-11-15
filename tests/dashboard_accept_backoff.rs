//! The dashboard websocket listener must back off on resource errors.
//!
//! Own test binary: it lowers the process's open-file limit.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustix::process::{Resource, getrlimit, setrlimit};
use tracing_subscriber::layer::SubscriberExt;

/// Counts `WARN` events from the dashboard publisher.
struct WarnCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCounter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if *meta.level() == tracing::Level::WARN && meta.target().contains("tui::remote") {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn exhaust_descriptors_but_one(limit: u64) -> Vec<std::fs::File> {
    let mut rlimit = getrlimit(Resource::Nofile);
    rlimit.current = Some(limit);
    setrlimit(Resource::Nofile, rlimit).expect("lower RLIMIT_NOFILE");
    let mut held = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        held.push(file);
    }
    assert!(!held.is_empty(), "limit {limit} left no free descriptor");
    held.pop();
    held
}

#[tokio::test(flavor = "current_thread")]
async fn dashboard_listener_backs_off_on_descriptor_exhaustion() {
    let warnings = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(WarnCounter(Arc::clone(&warnings)));
    let _guard = tracing::subscriber::set_default(subscriber);

    let proxy = nntp_proxy::NntpProxy::new_sync(
        nntp_proxy::create_default_config(),
        nntp_proxy::RoutingMode::PerCommand,
    )
    .unwrap();
    let app = nntp_proxy::tui::TuiAppBuilder::new(
        proxy.metrics().clone(),
        proxy.router().clone(),
        Arc::from(proxy.servers()),
    )
    .build();
    let listener = nntp_proxy::tui::bind_dashboard_listener("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let publisher = tokio::spawn(nntp_proxy::tui::run_dashboard_publisher_on_listener(
        app,
        listener,
        shutdown_rx,
    ));
    // Let the publisher start before descriptors run out.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before = warnings.load(Ordering::Relaxed);

    let original = getrlimit(Resource::Nofile);
    let open_now = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let held = exhaust_descriptors_but_one(open_now + 16);
    // Takes the last free slot; every accept() now fails with EMFILE.
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(held);
    setrlimit(Resource::Nofile, original).expect("restore RLIMIT_NOFILE");

    let during = warnings.load(Ordering::Relaxed) - before;
    assert!(
        during <= 3,
        "dashboard listener logged {during} warnings in 300 ms of EMFILE instead of backing off"
    );

    drop(client);
    shutdown_tx.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), publisher)
        .await
        .expect("publisher stops after shutdown")
        .unwrap()
        .unwrap();
}
