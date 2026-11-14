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
use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use nntp_proxy::config::{Cache, Routing, Server, StatefulSessions};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::{BackendId, MaxConnections, MessageId, Port};
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
    /// Refuses with a non-430 error, as some providers do with `451`.
    Refuses,
    /// Has the article, with a BODY large enough to outlast a client
    /// that disconnects after the status line.
    HasLarge,
}

/// Reply to an internal `STAT <message-id>` probe, when it differs from the
/// backend's normal lookup reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Probe {
    Found,
    Missing,
    Unavailable,
}

/// Replies a test may change while the proxy runs.
struct Control {
    lookup: Mutex<Lookup>,
    probe: Mutex<Option<Probe>>,
    /// When set, a `STAT <message-id>` reply waits for a permit.
    probe_gate: Mutex<Option<Arc<Semaphore>>>,
}

struct Backend {
    name: &'static str,
    tier: u8,
    stat_missing: bool,
    stateful_sessions: StatefulSessions,
    control: Arc<Control>,
}

impl Backend {
    fn new(name: &'static str, tier: u8, lookup: Lookup, sessions: StatefulSessions) -> Self {
        Self {
            name,
            tier,
            stat_missing: false,
            stateful_sessions: sessions,
            control: Arc::new(Control {
                lookup: Mutex::new(lookup),
                probe: Mutex::new(None),
                probe_gate: Mutex::new(None),
            }),
        }
    }

    /// The only backend that may be pinned, so the test knows who owns GROUP.
    fn pinned(lookup: Lookup) -> Self {
        Self::new("pinned", 0, lookup, StatefulSessions::Allowed)
    }

    fn other(name: &'static str, tier: u8, lookup: Lookup) -> Self {
        Self::new(name, tier, lookup, StatefulSessions::Excluded)
    }

    /// Enable `stat_missing` with the given reply to `STAT` probes.
    fn probed(mut self, probe: Probe) -> Self {
        self.stat_missing = true;
        *self.control.probe.lock().unwrap() = Some(probe);
        self
    }

    fn set_lookup(&self, lookup: Lookup) {
        *self.control.lookup.lock().unwrap() = lookup;
    }

    fn hold_probe_replies(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.control.probe_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

const LARGE_BODY_LEN: usize = 16 * 1024 * 1024;

fn lookup_reply(name: &str, verb: &str) -> String {
    match verb {
        "ARTICLE" => format!("220 0 {ID}\r\nSubject: from {name}\r\n\r\nbody from {name}\r\n.\r\n"),
        "HEAD" => format!("221 0 {ID}\r\nSubject: from {name}\r\n.\r\n"),
        "BODY" => format!("222 0 {ID}\r\nbody from {name}\r\n.\r\n"),
        "STAT" => format!("223 0 {ID}\r\n"),
        _ => unreachable!("lookup verbs only"),
    }
}

fn large_body_reply() -> String {
    format!("222 0 {ID}\r\n{}\r\n.\r\n", "x".repeat(LARGE_BODY_LEN))
}

fn missing_reply(name: &str) -> String {
    format!("430 not on {name}\r\n")
}

fn pointer_reply(name: &str, article: u64) -> String {
    format!("223 {article} <n{article}@{name}>\r\n")
}

fn group_reply() -> &'static str {
    "211 3 40 42 alt.test\r\n"
}

fn probe_reply(name: &str, probe: Probe) -> String {
    match probe {
        Probe::Found => lookup_reply(name, "STAT"),
        Probe::Missing => missing_reply(name),
        Probe::Unavailable => "503 probe unavailable\r\n".to_string(),
    }
}

fn lookup_response(name: &str, verb: &str, lookup: Lookup) -> String {
    match lookup {
        Lookup::Has => lookup_reply(name, verb),
        Lookup::HasLarge if verb == "BODY" => large_body_reply(),
        Lookup::HasLarge => lookup_reply(name, verb),
        Lookup::Missing => missing_reply(name),
        Lookup::Refuses => format!("451 0 {ID}\r\n"),
    }
}

/// A reader backend whose article pointer is per connection, like a real server.
async fn spawn_backend(backend: &Backend, log: CommandLog) -> Result<(u16, AbortHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let name = backend.name;
    let control = backend.control.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = log.clone();
            let control = control.clone();
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
                        Some(("STAT", id)) if id == ID => {
                            let gate = control.probe_gate.lock().unwrap().clone();
                            if let Some(gate) = gate {
                                gate.acquire().await.unwrap().forget();
                            }
                            let probe = *control.probe.lock().unwrap();
                            let lookup = *control.lookup.lock().unwrap();
                            probe.map_or_else(
                                || lookup_response(name, "STAT", lookup),
                                |probe| probe_reply(name, probe),
                            )
                        }
                        Some((verb, id))
                            if id == ID && matches!(verb, "ARTICLE" | "HEAD" | "BODY") =>
                        {
                            let lookup = *control.lookup.lock().unwrap();
                            lookup_response(name, verb, lookup)
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
                    if command == "QUIT" {
                        return;
                    }
                }
            });
        }
    });
    Ok((port, task.abort_handle()))
}

struct Options {
    failover: bool,
    mode: RoutingMode,
    /// Payload cache TTL; `None` runs without a payload cache.
    cache_ttl: Option<Duration>,
}

impl Options {
    const fn failover() -> Self {
        Self {
            failover: true,
            mode: RoutingMode::Hybrid,
            cache_ttl: None,
        }
    }

    const fn with_cache(mut self, ttl: Duration) -> Self {
        self.cache_ttl = Some(ttl);
        self
    }
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
        Self::start_with(
            backends,
            Options {
                failover,
                ..Options::failover()
            },
        )
        .await
    }

    async fn start_with(backends: &[Backend], options: Options) -> Result<Self> {
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
                    .stat_missing(u8::from(backend.stat_missing))
                    .max_connections(MaxConnections::try_new(1)?)
                    .compress(Some(false))
                    .build()?,
            );
        }
        let config = Config {
            servers,
            routing: Routing {
                reader_message_id_failover: options.failover,
                ..Routing::default()
            },
            cache: options.cache_ttl.map(|ttl| Cache {
                store_article_bodies: true,
                article_cache_ttl_secs: ttl,
                ..Cache::default()
            }),
            ..Config::default()
        };
        let proxy = NntpProxy::builder(config)
            .with_routing_mode(options.mode)
            .build()
            .await?;
        let port =
            spawn_test_proxy_on_random_port(proxy.clone(), options.mode == RoutingMode::Hybrid)
                .await?;
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

    /// How many times `name` received `verb <message-id>`.
    fn lookups_for(&self, name: &str, verb: &str) -> usize {
        let lookup = format!("{verb} {ID}");
        self.commands_for(name)
            .iter()
            .filter(|command| **command == lookup)
            .count()
    }

    /// Message-ID commands `name` received, in order.
    fn message_id_commands_for(&self, name: &str) -> Vec<String> {
        self.commands_for(name)
            .into_iter()
            .filter(|command| command.ends_with(ID))
            .collect()
    }

    /// Wait until the payload cache can answer `kind` for the article.
    async fn wait_cached(&self, kind: RequestKind) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if self
                    .proxy
                    .cache()
                    .get_request_message_id(ID)
                    .await
                    .is_some_and(|entry| entry.cached_response_for(kind, ID).is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
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

#[tokio::test]
async fn pinned_refusal_is_answered_by_a_same_tier_peer_without_recording_a_miss() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Refuses),
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
    harness
        .exchange("STAT", &pointer_reply("pinned", 40))
        .await?;

    assert_eq!(harness.backends_sent(&lookup), ["pinned", "peer"]);
    assert_eq!(
        harness.missing_bits().await,
        0,
        "a refusal is not a 430, so no backend is recorded missing"
    );
    Ok(())
}

#[tokio::test]
async fn pinned_refusal_does_not_open_a_lower_tier() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Refuses),
            Backend::other("fallback", 1, Lookup::Has),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    let lookup = format!("BODY {ID}");
    harness
        .exchange(
            &lookup,
            std::str::from_utf8(nntp_proxy::protocol::BACKEND_ERROR)?,
        )
        .await?;
    harness
        .exchange("STAT", &pointer_reply("pinned", 40))
        .await?;

    assert_eq!(harness.backends_sent(&lookup), ["pinned"]);
    assert_eq!(harness.missing_bits().await, 0);
    Ok(())
}

#[test]
fn failover_is_off_unless_configured() -> Result<()> {
    assert!(!Routing::default().reader_message_id_failover);
    let routing: Routing = toml::from_str("reader_message_id_failover = true")?;
    assert!(routing.reader_message_id_failover);
    Ok(())
}

// Recovered from archive/fork-main-2026-11-07: cache, selected-probe,
// disconnect, and byte-accounting guards for reader recovery.

const CACHE_TTL: Duration = Duration::from_secs(300);

#[tokio::test]
async fn recovery_serves_each_cached_variant_without_backend_traffic() -> Result<()> {
    for (warm_verb, verb, kind) in [
        ("BODY", "BODY", RequestKind::Body),
        ("ARTICLE", "HEAD", RequestKind::Head),
        ("ARTICLE", "ARTICLE", RequestKind::Article),
        ("STAT", "STAT", RequestKind::Stat),
    ] {
        let backends = [
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ];
        let mut harness =
            Harness::start_with(&backends, Options::failover().with_cache(CACHE_TTL)).await?;
        // Warm the cache through per-command routing before any GROUP.
        harness
            .exchange(
                &format!("{warm_verb} {ID}"),
                &lookup_reply("secondary", warm_verb),
            )
            .await?;
        harness.wait_cached(kind).await?;
        // A backup that has since lost the article proves the reply came
        // from the cache: asking it would answer 430.
        backends[1].set_lookup(Lookup::Missing);

        // Hybrid flushes the pinned session's bytes lazily, so measure
        // from before GROUP.
        let before = harness.proxy.metrics().snapshot(None);
        harness.select_group().await?;
        let secondary_commands = harness.commands_for("secondary").len();
        let expected = lookup_reply("secondary", verb);
        harness.exchange(&format!("{verb} {ID}"), &expected).await?;
        let after = harness.proxy.metrics().snapshot(None);

        assert_eq!(
            harness.commands_for("secondary").len(),
            secondary_commands,
            "{verb}: a cached response needs no backup traffic"
        );
        assert_eq!(
            after.backend_stats[1].bytes_received.as_u64(),
            before.backend_stats[1].bytes_received.as_u64(),
            "{verb}: cache bytes are never charged to a provider"
        );
        assert_eq!(
            after.user_stats[0].bytes_received.as_u64()
                - before.user_stats[0].bytes_received.as_u64(),
            (group_reply().len() + expected.len()) as u64,
            "{verb}: the user is charged for the cached reply once"
        );
        harness
            .exchange("STAT", &pointer_reply("pinned", 40))
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn recovery_from_a_cached_body_serves_body_and_stat_but_fetches_head_and_article()
-> Result<()> {
    for (verb, served_from_cache) in [
        ("BODY", true),
        ("STAT", true),
        ("HEAD", false),
        ("ARTICLE", false),
    ] {
        let backends = [
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has),
        ];
        let mut harness =
            Harness::start_with(&backends, Options::failover().with_cache(CACHE_TTL)).await?;
        harness
            .exchange(&format!("BODY {ID}"), &lookup_reply("secondary", "BODY"))
            .await?;
        harness.wait_cached(RequestKind::Body).await?;

        harness.select_group().await?;
        let before = harness.lookups_for("secondary", verb);
        harness
            .exchange(&format!("{verb} {ID}"), &lookup_reply("secondary", verb))
            .await?;
        assert_eq!(
            harness.lookups_for("secondary", verb) - before,
            usize::from(!served_from_cache),
            "{verb}: a cached BODY answers only BODY and STAT"
        );
        assert_eq!(
            harness.missing_bits().await & 0b01,
            0b01,
            "{verb}: the pinned miss is recorded"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_negative_cache_cannot_manufacture_a_payload_hit() -> Result<()> {
    let mut harness = Harness::start_with(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Missing),
        ],
        Options::failover().with_cache(CACHE_TTL),
    )
    .await?;
    let not_found = std::str::from_utf8(nntp_proxy::protocol::NO_SUCH_ARTICLE)?;
    harness.exchange(&format!("BODY {ID}"), not_found).await?;
    harness.select_group().await?;
    harness.exchange(&format!("BODY {ID}"), not_found).await?;

    let entry = harness
        .proxy
        .cache()
        .get_request_message_id(ID)
        .await
        .expect("misses are cached");
    assert!(!entry.is_complete_article());
    assert!(
        entry.cached_response_for(RequestKind::Body, ID).is_none(),
        "recorded 430s must never turn into a cached payload"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_refetches_an_expired_cached_payload() -> Result<()> {
    let backends = [
        Backend::pinned(Lookup::Missing),
        Backend::other("secondary", 1, Lookup::Has),
    ];
    let mut harness = Harness::start_with(
        &backends,
        Options::failover().with_cache(Duration::from_millis(10)),
    )
    .await?;
    harness
        .exchange(&format!("BODY {ID}"), &lookup_reply("secondary", "BODY"))
        .await?;
    harness.wait_cached(RequestKind::Body).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;

    harness.select_group().await?;
    harness
        .exchange(&format!("BODY {ID}"), &lookup_reply("secondary", "BODY"))
        .await?;
    assert_eq!(
        harness.lookups_for("secondary", "BODY"),
        2,
        "an expired payload must be fetched again, not served"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_counts_frontend_command_bytes_once_with_or_without_a_cache_hit() -> Result<()> {
    for cache_hit in [false, true] {
        let options = if cache_hit {
            Options::failover().with_cache(CACHE_TTL)
        } else {
            Options::failover()
        };
        let mut harness = Harness::start_with(
            &[
                Backend::pinned(Lookup::Missing),
                Backend::other("secondary", 1, Lookup::Has),
            ],
            options,
        )
        .await?;
        let body = lookup_reply("secondary", "BODY");
        if cache_hit {
            harness.exchange(&format!("BODY {ID}"), &body).await?;
            harness.wait_cached(RequestKind::Body).await?;
        }
        let before = harness.proxy.metrics().snapshot(None);
        harness.select_group().await?;
        harness.exchange(&format!("BODY {ID}"), &body).await?;
        harness.exchange("QUIT", "205 bye\r\n").await?;
        assert_eq!(harness.client.read(&mut [0]).await?, 0);
        let after = harness.proxy.metrics().snapshot(None);

        let frontend_bytes = format!("GROUP alt.test\r\nBODY {ID}\r\nQUIT\r\n").len() as u64;
        let resend_bytes = if cache_hit {
            0
        } else {
            format!("BODY {ID}\r\n").len() as u64
        };
        let sent = |stats: &nntp_proxy::metrics::MetricsSnapshot, backend: usize| {
            stats.backend_stats[backend].bytes_sent.as_u64()
        };
        assert_eq!(
            after.user_stats[0].bytes_sent.as_u64() - before.user_stats[0].bytes_sent.as_u64(),
            frontend_bytes,
            "cache_hit={cache_hit}: the user sent each command once"
        );
        assert_eq!(
            sent(&after, 0) - sent(&before, 0),
            frontend_bytes,
            "cache_hit={cache_hit}"
        );
        assert_eq!(
            sent(&after, 1) - sent(&before, 1),
            resend_bytes,
            "cache_hit={cache_hit}: only a real recovery resends to the backup"
        );
        assert_eq!(
            after.client_to_backend_bytes.as_u64() - before.client_to_backend_bytes.as_u64(),
            frontend_bytes + resend_bytes,
            "cache_hit={cache_hit}"
        );
        assert_eq!(
            after.user_stats[0].bytes_received.as_u64()
                - before.user_stats[0].bytes_received.as_u64(),
            (group_reply().len() + body.len() + "205 bye\r\n".len()) as u64,
            "cache_hit={cache_hit}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_honors_the_first_alternate_stat_missing_probe() -> Result<()> {
    let body = format!("BODY {ID}");
    let stat = format!("STAT {ID}");
    for (probe, expected_commands) in [
        (None, vec![body.clone()]),
        (Some(Probe::Found), vec![stat.clone(), body.clone()]),
        (Some(Probe::Missing), vec![stat.clone()]),
        (Some(Probe::Unavailable), vec![stat.clone(), body.clone()]),
    ] {
        let secondary = Backend::other("secondary", 1, Lookup::Has);
        let secondary = match probe {
            Some(probe) => secondary.probed(probe),
            None => secondary,
        };
        let mut harness =
            Harness::start(&[Backend::pinned(Lookup::Missing), secondary], true).await?;
        harness.select_group().await?;
        let probe_missing = probe == Some(Probe::Missing);
        let expected = if probe_missing {
            std::str::from_utf8(nntp_proxy::protocol::NO_SUCH_ARTICLE)?.to_string()
        } else {
            lookup_reply("secondary", "BODY")
        };
        harness.exchange(&body, &expected).await?;

        assert_eq!(
            harness.message_id_commands_for("secondary"),
            expected_commands,
            "probe={probe:?}"
        );
        assert_eq!(
            harness.missing_bits().await,
            if probe_missing { 0b11 } else { 0b01 },
            "probe={probe:?}: only a probe 430 records the backup missing"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovered_stat_lookup_is_not_probed_with_stat_again() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::Has).probed(Probe::Found),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness
        .exchange(&format!("STAT {ID}"), &lookup_reply("secondary", "STAT"))
        .await?;
    assert_eq!(harness.lookups_for("secondary", "STAT"), 1);
    Ok(())
}

#[tokio::test]
async fn only_a_same_tier_probe_430_opens_the_lower_tier() -> Result<()> {
    for peer_probe in [Probe::Missing, Probe::Unavailable] {
        let opens = peer_probe == Probe::Missing;
        let mut harness = Harness::start(
            &[
                Backend::pinned(Lookup::Missing),
                Backend::other("peer", 0, Lookup::Refuses).probed(peer_probe),
                Backend::other("lower", 1, Lookup::Has).probed(Probe::Found),
            ],
            true,
        )
        .await?;
        harness.select_group().await?;
        let expected = if opens {
            lookup_reply("lower", "BODY")
        } else {
            std::str::from_utf8(nntp_proxy::protocol::BACKEND_ERROR)?.to_string()
        };
        harness.exchange(&format!("BODY {ID}"), &expected).await?;

        assert_eq!(
            harness.lookups_for("peer", "BODY"),
            usize::from(!opens),
            "{peer_probe:?}: an unconfirmed peer still gets the original BODY"
        );
        assert_eq!(
            harness.lookups_for("lower", "BODY"),
            usize::from(opens),
            "{peer_probe:?}: only a same-tier 430 may open the lower tier"
        );
        assert_eq!(
            harness.missing_bits().await,
            if opens { 0b011 } else { 0b001 },
            "{peer_probe:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn current_tier_hit_does_not_wait_for_a_held_lower_tier_stat() -> Result<()> {
    let backends = [
        Backend::pinned(Lookup::Missing),
        Backend::other("current", 0, Lookup::Has).probed(Probe::Found),
        Backend::other("lower", 1, Lookup::Has).probed(Probe::Found),
    ];
    let held = backends[2].hold_probe_replies();
    let mut harness = Harness::start(&backends, true).await?;
    harness.select_group().await?;
    harness.send(&format!("BODY {ID}\r\n")).await?;
    // `expect` is bounded, so a recovery that waits on the held lower-tier
    // STAT fails here instead of hanging.
    harness.expect(&lookup_reply("current", "BODY")).await?;
    assert_eq!(held.available_permits(), 0);
    held.add_permits(16);

    assert_eq!(
        harness.message_id_commands_for("current"),
        [format!("STAT {ID}"), format!("BODY {ID}")]
    );
    assert_eq!(harness.lookups_for("lower", "BODY"), 0);
    Ok(())
}

#[tokio::test]
async fn client_disconnect_during_recovery_releases_router_load_and_pool_slots() -> Result<()> {
    let mut harness = Harness::start(
        &[
            Backend::pinned(Lookup::Missing),
            Backend::other("secondary", 1, Lookup::HasLarge),
        ],
        true,
    )
    .await?;
    harness.select_group().await?;
    harness.send(&format!("BODY {ID}\r\n")).await?;
    harness.expect(&format!("222 0 {ID}\r\n")).await?;
    socket2::SockRef::from(&harness.client).set_linger(Some(Duration::ZERO))?;
    // Dropping the harness resets the client mid-body. Backend connection
    // tasks outlive it; only their accept loops stop.
    let proxy = harness.proxy.clone();
    drop(harness);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if (0..2).all(|index| {
                let backend = BackendId::from_index(index);
                let status = proxy
                    .router()
                    .backend_provider(backend)
                    .unwrap()
                    .status_counts();
                proxy.router().backend_load(backend).unwrap().get() == 0
                    && status.size == status.available
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("recovery guards were not released after the client reset"))?;
    Ok(())
}

#[tokio::test]
async fn hybrid_and_stateful_count_every_frontend_command_byte_once() -> Result<()> {
    for (mode, prelude) in [
        (RoutingMode::Hybrid, false),
        (RoutingMode::Hybrid, true),
        (RoutingMode::Stateful, false),
        (RoutingMode::Stateful, true),
    ] {
        let mut harness = Harness::start_with(
            &[Backend::pinned(Lookup::Has)],
            Options {
                failover: false,
                mode,
                cache_ttl: None,
            },
        )
        .await?;
        let before = harness.proxy.metrics().snapshot(None);
        if prelude {
            harness.exchange("DATE", "111 20260101000000\r\n").await?;
        }
        harness.select_group().await?;
        let body = lookup_reply("pinned", "BODY");
        harness.exchange(&format!("BODY {ID}"), &body).await?;
        harness.exchange("QUIT", "205 bye\r\n").await?;
        assert_eq!(harness.client.read(&mut [0]).await?, 0);
        let after = harness.proxy.metrics().snapshot(None);

        let frontend_bytes = format!("GROUP alt.test\r\nBODY {ID}\r\nQUIT\r\n").len() as u64
            + if prelude { "DATE\r\n".len() as u64 } else { 0 };
        let response_bytes = (group_reply().len()
            + body.len()
            + "205 bye\r\n".len()
            + if prelude {
                "111 20260101000000\r\n".len()
            } else {
                0
            }) as u64;
        assert_eq!(
            after.user_stats[0].bytes_sent.as_u64() - before.user_stats[0].bytes_sent.as_u64(),
            frontend_bytes,
            "{mode:?} prelude={prelude}: user bytes sent"
        );
        assert_eq!(
            after.client_to_backend_bytes.as_u64() - before.client_to_backend_bytes.as_u64(),
            frontend_bytes,
            "{mode:?} prelude={prelude}: client to backend bytes"
        );
        assert_eq!(
            after.backend_stats[0].bytes_sent.as_u64()
                - before.backend_stats[0].bytes_sent.as_u64(),
            frontend_bytes,
            "{mode:?} prelude={prelude}: backend bytes sent"
        );
        assert_eq!(
            after.backend_stats[0].bytes_received.as_u64()
                - before.backend_stats[0].bytes_received.as_u64(),
            response_bytes,
            "{mode:?} prelude={prelude}: backend bytes received"
        );
        assert_eq!(
            after.backend_to_client_bytes.as_u64() - before.backend_to_client_bytes.as_u64(),
            response_bytes,
            "{mode:?} prelude={prelude}: backend to client bytes"
        );
    }
    Ok(())
}
