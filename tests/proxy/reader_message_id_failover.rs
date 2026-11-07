//! Hybrid reader sessions recovering message-ID misses from other backends.
//!
//! An indexer selects a group on its pinned backend and then fetches PAR2/NFO
//! metadata by message-ID on the same connection. With
//! `[routing] reader_message_id_failover = true`, a `430` from the pinned
//! backend is answered from another backend while the pinned connection keeps
//! the client's group and article pointer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use nntp_proxy::config::{Routing, Server, StatefulSessions};
use nntp_proxy::types::{MaxConnections, MessageId, Port};
use nntp_proxy::{Config, NntpProxy, RoutingMode};

use crate::test_helpers::{
    connect_and_read_greeting, independent_provider_host, spawn_test_proxy_on_random_port,
};

const ID: &str = "<par2@example.com>";

/// Commands seen by every mock backend, in arrival order across backends.
type CommandLog = Arc<Mutex<Vec<(&'static str, String)>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Has,
    Missing,
}

struct Backend {
    name: &'static str,
    tier: u8,
    lookup: Lookup,
    stateful_sessions: StatefulSessions,
}

impl Backend {
    /// The only backend that may be pinned, so the test knows who owns GROUP.
    const fn pinned(lookup: Lookup) -> Self {
        Self {
            name: "pinned",
            tier: 0,
            lookup,
            stateful_sessions: StatefulSessions::Allowed,
        }
    }

    const fn other(name: &'static str, tier: u8, lookup: Lookup) -> Self {
        Self {
            name,
            tier,
            lookup,
            stateful_sessions: StatefulSessions::Excluded,
        }
    }
}

fn lookup_reply(name: &str, verb: &str) -> String {
    match verb {
        "ARTICLE" => format!("220 0 {ID}\r\nSubject: from {name}\r\n\r\nbody from {name}\r\n.\r\n"),
        "HEAD" => format!("221 0 {ID}\r\nSubject: from {name}\r\n.\r\n"),
        "BODY" => format!("222 0 {ID}\r\nbody from {name}\r\n.\r\n"),
        "STAT" => format!("223 0 {ID}\r\n"),
        _ => unreachable!("lookup verbs only"),
    }
}

fn missing_reply(name: &str) -> String {
    format!("430 not on {name}\r\n")
}

fn pointer_reply(name: &str, article: u64) -> String {
    format!("223 {article} <n{article}@{name}>\r\n")
}

/// A reader backend whose article pointer is per connection, like a real server.
async fn spawn_backend(backend: &Backend, log: CommandLog) -> Result<(u16, AbortHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let name = backend.name;
    let lookup = backend.lookup;
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                if stream.get_mut().write_all(b"200 ready\r\n").await.is_err() {
                    return;
                }
                let mut article = 0;
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let command = line.trim_end().to_string();
                    log.lock().unwrap().push((name, command.clone()));
                    let reply = match command.split_once(' ') {
                        Some((verb, id))
                            if id == ID && matches!(verb, "ARTICLE" | "HEAD" | "BODY" | "STAT") =>
                        {
                            match lookup {
                                Lookup::Has => lookup_reply(name, verb),
                                Lookup::Missing => missing_reply(name),
                            }
                        }
                        Some(("GROUP", group)) => {
                            article = 40;
                            format!("211 3 40 42 {group}\r\n")
                        }
                        _ => match command.as_str() {
                            "NEXT" => {
                                article += 1;
                                pointer_reply(name, article)
                            }
                            "STAT" => pointer_reply(name, article),
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
    Ok((port, task.abort_handle()))
}

struct Harness {
    proxy: NntpProxy,
    client: TcpStream,
    log: CommandLog,
    backends: Vec<AbortHandle>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for backend in &self.backends {
            backend.abort();
        }
    }
}

impl Harness {
    async fn start(backends: &[Backend], failover: bool) -> Result<Self> {
        let log = CommandLog::default();
        let mut servers = Vec::new();
        let mut handles = Vec::new();
        for (index, backend) in backends.iter().enumerate() {
            let (port, handle) = spawn_backend(backend, log.clone()).await?;
            handles.push(handle);
            // Distinct hosts give each backend its own availability slot.
            servers.push(
                Server::builder(independent_provider_host(index), Port::try_new(port)?)
                    .name(backend.name)
                    .tier(backend.tier)
                    .stateful_sessions(backend.stateful_sessions)
                    .max_connections(MaxConnections::try_new(1)?)
                    .compress(Some(false))
                    .build()?,
            );
        }
        let config = Config {
            servers,
            routing: Routing {
                reader_message_id_failover: failover,
                ..Routing::default()
            },
            ..Config::default()
        };
        let proxy = NntpProxy::builder(config)
            .with_routing_mode(RoutingMode::Hybrid)
            .build()
            .await?;
        let port = spawn_test_proxy_on_random_port(proxy.clone(), true).await?;
        let client = connect_and_read_greeting(port).await?;
        Ok(Self {
            proxy,
            client,
            log,
            backends: handles,
        })
    }

    async fn send(&mut self, commands: &str) -> Result<()> {
        self.client.write_all(commands.as_bytes()).await?;
        self.client.flush().await?;
        Ok(())
    }

    /// Read exactly `expected`, so a stray or reordered reply fails the test.
    async fn expect(&mut self, expected: &str) -> Result<()> {
        let mut actual = vec![0; expected.len()];
        tokio::time::timeout(Duration::from_secs(5), self.client.read_exact(&mut actual)).await??;
        assert_eq!(String::from_utf8_lossy(&actual), expected);
        Ok(())
    }

    async fn exchange(&mut self, command: &str, expected: &str) -> Result<()> {
        self.send(&format!("{command}\r\n")).await?;
        self.expect(expected).await
    }

    async fn select_group(&mut self) -> Result<()> {
        self.exchange("GROUP alt.test", "211 3 40 42 alt.test\r\n")
            .await
    }

    fn commands_for(&self, name: &str) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(backend, _)| *backend == name)
            .map(|(_, command)| command.clone())
            .collect()
    }

    /// Backends that received `command`, in the order they received it.
    fn backends_sent(&self, command: &str) -> Vec<&'static str> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, logged)| logged == command)
            .map(|(backend, _)| *backend)
            .collect()
    }

    /// Availability slots recorded missing; backend `i` owns bit `i`.
    async fn missing_bits(&self) -> usize {
        self.proxy.cache().sync().await;
        self.proxy
            .cache()
            .get(&MessageId::from_borrowed(ID).unwrap())
            .await
            .map_or(0, |entry| entry.availability().missing_bits())
    }
}

#[tokio::test]
async fn pinned_miss_is_answered_by_another_backend_for_every_lookup_verb() -> Result<()> {
    for verb in ["ARTICLE", "HEAD", "BODY", "STAT"] {
        let mut harness = Harness::start(
            &[
                Backend::pinned(Lookup::Missing),
                Backend::other("secondary", 1, Lookup::Has),
            ],
            true,
        )
        .await?;
        harness.select_group().await?;
        let lookup = format!("{verb} {ID}");
        harness
            .exchange(&lookup, &lookup_reply("secondary", verb))
            .await?;

        assert_eq!(
            harness.backends_sent(&lookup),
            ["pinned", "secondary"],
            "{verb}: the pinned backend answers first"
        );
        assert_eq!(
            harness.missing_bits().await,
            0b01,
            "{verb}: only the backend that returned 430 is recorded missing"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_leaves_group_and_article_pointer_on_the_pinned_backend() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness
        .exchange("NEXT", &pointer_reply("pinned", 41))
        .await?;
    harness
        .exchange(&format!("BODY {ID}"), &lookup_reply("secondary", "BODY"))
        .await?;
    harness
        .exchange("STAT", &pointer_reply("pinned", 41))
        .await?;
    harness
        .exchange("NEXT", &pointer_reply("pinned", 42))
        .await?;

    let secondary = harness.commands_for("secondary");
    assert!(secondary.contains(&format!("BODY {ID}")));
    assert!(
        secondary
            .iter()
            .all(|command| !command.starts_with("GROUP") && command != "NEXT" && command != "STAT"),
        "reader state must never move to the recovering backend: {secondary:?}"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_charges_the_client_command_to_the_user_once() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    let reply = lookup_reply("secondary", "BODY");
    harness.exchange(&format!("BODY {ID}"), &reply).await?;

    // Recovery flushes the pinned backend's bytes before reporting its own.
    let snapshot = harness.proxy.metrics().snapshot(None);
    let [user] = snapshot.user_stats.as_slice() else {
        panic!("expected one user, got {:?}", snapshot.user_stats);
    };
    let pinned = &snapshot.backend_stats[0];
    let secondary = &snapshot.backend_stats[1];
    let command_len = format!("BODY {ID}\r\n").len() as u64;
    assert_eq!(secondary.bytes_sent.as_u64(), command_len);
    assert_eq!(secondary.bytes_received.as_u64(), reply.len() as u64);
    // Every client command went to the pinned backend; the recovery's resend
    // is provider traffic, not a second user command.
    assert_eq!(user.bytes_sent.as_u64(), pinned.bytes_sent.as_u64());
    assert_eq!(
        user.bytes_received.as_u64(),
        pinned.bytes_received.as_u64() + reply.len() as u64
    );
    Ok(())
}

#[tokio::test]
async fn pipelined_recovery_answers_in_request_order() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness
        .send(&format!("NEXT\r\nBODY {ID}\r\nSTAT {ID}\r\nSTAT\r\n"))
        .await?;
    harness.expect(&pointer_reply("pinned", 41)).await?;
    harness.expect(&lookup_reply("secondary", "BODY")).await?;
    harness.expect(&lookup_reply("secondary", "STAT")).await?;
    harness.expect(&pointer_reply("pinned", 41)).await?;
    Ok(())
}

#[tokio::test]
async fn pinned_hit_never_reaches_another_backend() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Has),
            Backend::other("secondary", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness
        .exchange(&format!("BODY {ID}"), &lookup_reply("pinned", "BODY"))
        .await?;

    assert!(
        harness
            .commands_for("secondary")
            .iter()
            .all(|command| !command.contains(ID))
    );
    assert_eq!(harness.missing_bits().await, 0);
    Ok(())
}

#[tokio::test]
async fn disabled_failover_forwards_the_pinned_miss() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ],
        false,
    )
    .await?;
    harness.select_group().await?;
    harness
        .exchange(&format!("BODY {ID}"), &missing_reply("pinned"))
        .await?;
    harness
        .exchange("STAT", &pointer_reply("pinned", 40))
        .await?;

    assert!(
        harness
            .commands_for("secondary")
            .iter()
            .all(|command| !command.contains(ID))
    );
    assert_eq!(harness.missing_bits().await, 0);
    Ok(())
}

#[tokio::test]
async fn recovery_exhausts_the_current_tier_before_a_lower_one() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("peer", 0, Lookup::Missing),
            Backend::other("fallback", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    let lookup = format!("BODY {ID}");
    harness
        .exchange(&lookup, &lookup_reply("fallback", "BODY"))
        .await?;

    assert_eq!(
        harness.backends_sent(&lookup),
        ["pinned", "peer", "fallback"]
    );
    assert_eq!(harness.missing_bits().await, 0b011);
    Ok(())
}

#[tokio::test]
async fn recovery_stops_at_a_hit_in_the_current_tier() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("peer", 0, Lookup::Has),
            Backend::other("fallback", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    let lookup = format!("BODY {ID}");
    harness
        .exchange(&lookup, &lookup_reply("peer", "BODY"))
        .await?;

    assert_eq!(harness.backends_sent(&lookup), ["pinned", "peer"]);
    assert_eq!(harness.missing_bits().await, 0b001);
    Ok(())
}

#[tokio::test]
async fn exhausted_recovery_answers_430_and_keeps_the_session() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Missing),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness
        .exchange(
            &format!("BODY {ID}"),
            std::str::from_utf8(nntp_proxy::protocol::NO_SUCH_ARTICLE)?,
        )
        .await?;
    harness
        .exchange("STAT", &pointer_reply("pinned", 40))
        .await?;
    assert_eq!(harness.missing_bits().await, 0b11);
    Ok(())
}

#[test]
fn failover_is_off_unless_configured() -> Result<()> {
    assert!(!Routing::default().reader_message_id_failover);
    let routing: Routing = toml::from_str("reader_message_id_failover = true")?;
    assert!(routing.reader_message_id_failover);
    Ok(())
}
