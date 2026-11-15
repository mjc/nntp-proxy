//! The client accept loop must survive `accept()` errors.
//!
//! This lives in its own test binary because it lowers the process's
//! open-file limit, which would break unrelated tests sharing the process.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use rustix::process::{Resource, getrlimit, setrlimit};
use tokio::io::AsyncReadExt;

/// Hold every descriptor number below `limit` so the next `accept()` fails
/// with `EMFILE`, then release one slot for the test's own client socket.
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

async fn read_line(client: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0; 1];
    loop {
        client.read_exact(&mut byte).await.unwrap();
        line.push(byte[0]);
        if byte[0] == b'\n' {
            return line;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn accept_loop_survives_descriptor_exhaustion() {
    let proxy = Arc::new(
        nntp_proxy::NntpProxy::new_sync(
            nntp_proxy::create_default_config(),
            nntp_proxy::RoutingMode::PerCommand,
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let accept_loop = tokio::spawn(nntp_proxy::runtime::run_accept_loop(
        proxy,
        listener,
        shutdown_rx,
        nntp_proxy::RoutingMode::PerCommand,
    ));

    let original = getrlimit(Resource::Nofile);
    let open_now = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let held = exhaust_descriptors_but_one(open_now + 16);

    // Takes the one free slot; the proxy's accept() then has none.
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let died_while_exhausted = accept_loop.is_finished();

    drop(held);
    setrlimit(Resource::Nofile, original).expect("restore RLIMIT_NOFILE");

    assert!(
        !died_while_exhausted,
        "accept loop exited on EMFILE: {:?}",
        accept_loop.await
    );
    let greeting = tokio::time::timeout(Duration::from_secs(5), read_line(&mut client))
        .await
        .expect("the pending client is accepted once descriptors free up");
    assert!(
        nntp_proxy::protocol::StatusCode::parse(&greeting).is_some(),
        "{greeting:?}"
    );

    shutdown_tx.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), accept_loop)
        .await
        .expect("accept loop stops after shutdown")
        .unwrap()
        .unwrap();
}
