//! Client `STAT <message-id>` must fail over on non-430 refusals exactly like
//! ARTICLE/BODY/HEAD.
//!
//! Newsgroupdirect's backup server answers lookups it cannot serve with
//! `451 0 <message-id>`. A downloader that checks availability with `STAT`
//! before fetching must not see that refusal while another eligible backend
//! has the article, whichever routing path the request takes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use nntp_proxy::config::{Routing, Server};
use nntp_proxy::types::{MaxConnections, Port};
use nntp_proxy::{Config, NntpProxy, RoutingMode};

use crate::test_helpers::{
    connect_and_read_greeting, independent_provider_host, spawn_test_proxy_on_random_port,
};

#[derive(Clone, Copy)]
enum Lookup {
    Has,
    Missing,
    /// Refuses with a non-430 error, as some providers do with `451`.
    Refuses,
}

struct Backend {
    name: &'static str,
    tier: u8,
    lookup: Lookup,
}

/// A backend that answers every message-ID lookup the same way and counts
/// the lookups it received.
async fn spawn_backend(lookup: Lookup, lookups: Arc<AtomicUsize>) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let lookups = lookups.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                if stream.get_mut().write_all(b"200 ready\r\n").await.is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let command = line.trim_end().to_string();
                    let reply = match command.split_once(' ') {
                        Some((verb, id))
                            if matches!(verb, "ARTICLE" | "HEAD" | "BODY" | "STAT") =>
                        {
                            lookups.fetch_add(1, Ordering::Relaxed);
                            match (lookup, verb) {
                                (Lookup::Has, "STAT") => format!("223 0 {id}\r\n"),
                                (Lookup::Has, "HEAD") => {
                                    format!("221 0 {id}\r\nSubject: x\r\n.\r\n")
                                }
                                (Lookup::Has, "BODY") => format!("222 0 {id}\r\nbody\r\n.\r\n"),
                                (Lookup::Has, _) => {
                                    format!("220 0 {id}\r\nSubject: x\r\n\r\nbody\r\n.\r\n")
                                }
                                (Lookup::Missing, _) => "430 no such article\r\n".to_string(),
                                (Lookup::Refuses, _) => format!("451 0 {id}\r\n"),
                            }
                        }
                        _ => match command.as_str() {
                            "MODE READER" => "200 reader\r\n".to_string(),
                            "DATE" => "111 20260101000000\r\n".to_string(),
                            "QUIT" => "205 bye\r\n".to_string(),
                            _ => "500 unknown command\r\n".to_string(),
                        },
                    };
                    if stream.get_mut().write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(port)
}

/// Start a hybrid-mode proxy over `backends`, returning its port and each
/// backend's lookup counter.
async fn start_proxy(
    backends: &[Backend],
    routing: Routing,
) -> Result<(u16, Vec<Arc<AtomicUsize>>)> {
    let mut servers = Vec::new();
    let mut counters = Vec::new();
    for (index, backend) in backends.iter().enumerate() {
        let lookups = Arc::new(AtomicUsize::new(0));
        let port = spawn_backend(backend.lookup, lookups.clone()).await?;
        counters.push(lookups);
        // Distinct hosts give each backend its own availability slot.
        servers.push(
            Server::builder(independent_provider_host(index), Port::try_new(port)?)
                .name(backend.name)
                .tier(backend.tier)
                .max_connections(MaxConnections::try_new(2)?)
                .compress(Some(false))
                .build()?,
        );
    }
    let config = Config {
        servers,
        routing,
        ..Config::default()
    };
    let proxy = NntpProxy::builder(config)
        .with_routing_mode(RoutingMode::Hybrid)
        .build()
        .await?;
    let port = spawn_test_proxy_on_random_port(proxy, true).await?;
    Ok((port, counters))
}

/// Send one `STAT` per message-ID, each on a fresh client connection, and
/// return the status codes.
async fn stat_each_on_fresh_connection(port: u16, count: usize) -> Result<Vec<String>> {
    let mut codes = Vec::with_capacity(count);
    for index in 0..count {
        let client = connect_and_read_greeting(port).await?;
        let mut client = BufReader::new(client);
        client
            .get_mut()
            .write_all(format!("STAT <stat-{index}@example.com>\r\n").as_bytes())
            .await?;
        let mut line = String::new();
        client.read_line(&mut line).await?;
        codes.push(line.chars().take(3).collect());
    }
    Ok(codes)
}

fn assert_all_found(codes: &[String]) {
    assert!(
        codes.iter().all(|code| code == "223"),
        "every STAT should be answered by the backend that has the article, got {codes:?}"
    );
}

#[tokio::test]
async fn stat_refused_with_451_fails_over_to_same_tier_backend_in_hybrid_mode() -> Result<()> {
    let backends = [
        Backend {
            name: "refuses",
            tier: 0,
            lookup: Lookup::Refuses,
        },
        Backend {
            name: "has",
            tier: 0,
            lookup: Lookup::Has,
        },
    ];
    let (port, lookups) = start_proxy(&backends, Routing::default()).await?;

    let codes = stat_each_on_fresh_connection(port, 8).await?;

    assert_all_found(&codes);
    assert!(
        lookups[0].load(Ordering::Relaxed) > 0,
        "the refusing backend must have been asked for the test to mean anything"
    );
    Ok(())
}

#[tokio::test]
async fn stat_refused_in_backup_tier_fails_over_after_primary_tier_430s() -> Result<()> {
    // The tier-1 refuser is only reached after every tier-0 backend returned
    // 430, the path a release missing from the primaries takes.
    let backends = [
        Backend {
            name: "primary-a",
            tier: 0,
            lookup: Lookup::Missing,
        },
        Backend {
            name: "primary-b",
            tier: 0,
            lookup: Lookup::Missing,
        },
        Backend {
            name: "backup-refuses",
            tier: 1,
            lookup: Lookup::Refuses,
        },
        Backend {
            name: "backup-has",
            tier: 1,
            lookup: Lookup::Has,
        },
    ];
    let (port, lookups) = start_proxy(&backends, Routing::default()).await?;

    let codes = stat_each_on_fresh_connection(port, 8).await?;

    assert_all_found(&codes);
    assert!(
        lookups[2].load(Ordering::Relaxed) > 0,
        "backup refuser was never asked"
    );
    Ok(())
}

#[tokio::test]
async fn pipelined_stats_refused_with_451_fail_over_in_hybrid_mode() -> Result<()> {
    let backends = [
        Backend {
            name: "primary",
            tier: 0,
            lookup: Lookup::Missing,
        },
        Backend {
            name: "backup-refuses",
            tier: 1,
            lookup: Lookup::Refuses,
        },
        Backend {
            name: "backup-has",
            tier: 1,
            lookup: Lookup::Has,
        },
    ];
    let (port, lookups) = start_proxy(&backends, Routing::default()).await?;
    let client = connect_and_read_greeting(port).await?;
    let mut client = BufReader::new(client);

    let count = 12;
    let mut batch = String::new();
    for index in 0..count {
        batch.push_str(&format!("STAT <pipelined-{index}@example.com>\r\n"));
    }
    client.get_mut().write_all(batch.as_bytes()).await?;
    let mut codes = Vec::with_capacity(count);
    for _ in 0..count {
        let mut line = String::new();
        client.read_line(&mut line).await?;
        codes.push(line.chars().take(3).collect::<String>());
    }

    assert_all_found(&codes);
    assert!(
        lookups[1].load(Ordering::Relaxed) > 0,
        "backup refuser was never asked"
    );
    Ok(())
}

#[tokio::test]
async fn stat_refused_with_451_fails_over_with_adaptive_precheck() -> Result<()> {
    // Adaptive precheck races STAT across a tier before the retry loop runs;
    // a refusal there must leave the request to the retry loop, not reach the
    // client.
    let backends = [
        Backend {
            name: "refuses",
            tier: 0,
            lookup: Lookup::Refuses,
        },
        Backend {
            name: "has",
            tier: 0,
            lookup: Lookup::Has,
        },
    ];
    let routing = Routing {
        adaptive_precheck: true,
        ..Routing::default()
    };
    let (port, lookups) = start_proxy(&backends, routing).await?;

    let codes = stat_each_on_fresh_connection(port, 6).await?;

    assert_all_found(&codes);
    assert!(
        lookups[0].load(Ordering::Relaxed) > 0,
        "refuser was never asked"
    );
    Ok(())
}

#[tokio::test]
async fn stat_refused_by_every_backend_gets_backend_error_not_the_refusal() -> Result<()> {
    let backends = [
        Backend {
            name: "refuses-a",
            tier: 0,
            lookup: Lookup::Refuses,
        },
        Backend {
            name: "refuses-b",
            tier: 0,
            lookup: Lookup::Refuses,
        },
    ];
    let (port, _lookups) = start_proxy(&backends, Routing::default()).await?;

    let codes = stat_each_on_fresh_connection(port, 2).await?;

    // No backend said the article is missing, so the proxy must not claim
    // 430, and the provider-specific 451 must not leak through either.
    assert!(
        codes.iter().all(|code| code == "503"),
        "every backend refusing should give the no-backend reply, got {codes:?}"
    );
    Ok(())
}
