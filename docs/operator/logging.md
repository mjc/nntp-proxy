# Logging

Headless `nntp-proxy` writes its log to stdout. This page covers the output
formats, what each level means, the `event` values worth alerting on, and
example alert queries for log stores.

## Output formats

| `[logging] format` | Output |
| --- | --- |
| `"text"` (default) | One human-readable line per event: timestamp, level, target, message, then `key=value` fields |
| `"json"` | One JSON object per line |

Override the config with `--log-format json` or `NNTP_PROXY_LOG_FORMAT=json`.

Text output is coloured only when stdout is a terminal. Setting `NO_COLOR` to
any non-empty value turns colour off even on a terminal. Container log drivers
and pipes therefore always receive plain text.

The TUI log pane and the local `debug.log` stay human-readable text whatever
the format.

### JSON field names

| Field | Meaning |
| --- | --- |
| `timestamp` | RFC 3339 time the event was recorded |
| `level` | `ERROR`, `WARN`, `INFO`, `DEBUG`, or `TRACE` |
| `target` | Rust module that logged the event, such as `nntp_proxy::pool::provider` |
| `message` | Human-readable message |
| `span` | The innermost active span, with its `name` and fields. Absent when no span is active |
| any other key | Event fields, at the top level: `backend`, `pool`, `client`, `error`, and so on |

Example:

```json
{"timestamp":"2026-01-01T00:00:00.000000Z","level":"WARN","message":"Backend unreachable","event":"backend_unreachable","backend":"backend-a","host":"news.example.com","port":563,"count":1,"error":"I/O error: Connection refused (os error 111)","target":"nntp_proxy::pool::deadpool_connection"}
```

## Levels

Levels describe what an operator should do, not how unusual the code path is.

| Level | Meaning | Examples |
| --- | --- | --- |
| `ERROR` | Needs operator action; the proxy cannot fix it | Backend credentials rejected, persisted state cannot be written, a backend misconfigured |
| `WARN` | Abnormal but self-healing; alert when it persists | Backend unreachable, account connection limit reached, health checks failing, pool shrunk after errors, a client session ending on a backend error |
| `INFO` | Lifecycle, summaries, and recoveries | Startup, listener, busy-backend and periodic backend summaries, `<event>_cleared` lines |
| `DEBUG` | Per-request and per-connection detail | Client disconnects and resets, malformed client commands, expected `430`s, busy checkouts, stale pooled sockets, the first attempt of a retried command |

Client disconnects are normal and never logged above `DEBUG`. Situations that
repeat per request are counted and summarised instead of logged per request,
and the summary paths do no formatting work when their level is filtered out.

## Operator events

Situations worth an alert carry an `event` field with a stable value and a
stable message, so alert rules match the field instead of free text. Fields
listed for each event are present on every line for it.

Recurring conditions are rate limited per backend (or per file or pool): the
first occurrence logs at once, a persistent condition logs again at most about
once a minute, and `count` says how many times it happened since the previous
line. When the condition ends, one `<event>_cleared` line at `INFO` reports
`occurrences` (or a per-event name below) and `duration_secs`. A backend setup
event clears once both are true: something succeeded after its last failure,
and a full minute has passed since that failure. Each backend checks this
every 15 seconds, whether or not its pool has idle connections, so a
recovered backend whose pool stays full still logs its cleared line, and a
partly failing backend does not flap between failure and cleared lines. For
the login-type events (`backend_login_refused`, `backend_login_rejected`,
`backend_connection_limit`, `backend_unavailable`,
`backend_reader_mode_refused`, `backend_misconfigured`)
the success must be a new connection logging in; for `backend_unreachable` a
successful connect or a passing health-check cycle (every check in it
passed) also counts.

Refusals use the same classification as login backoff and the adaptive
connection cap (see
[Login refusals and connection limits](operations.md#login-refusals-and-connection-limits)):
`481` to `AUTHINFO` is `backend_login_refused`, whatever its text says; code
`482`, or a reply whose words name a connection limit ("too many
connections", "connection limit (100) reached", "max conns", and similar, but
never one about failed logins, a locked or expired account, a quota, or a
download or rate limit), is `backend_connection_limit`; any other non-2xx
greeting is `backend_unavailable`; any other refused `MODE READER` is
`backend_reader_mode_refused`; any other `AUTHINFO` refusal is
`backend_login_rejected`. Every line carries the backend's reply in `error`,
so read it when the event name is not specific enough.

Every refusal from a backend that answered also pauses new connections to it
for `retry_in_secs`, because an immediate retry gets the same answer.

| `event` | Level | Fields | Meaning | Suggested action |
| --- | --- | --- | --- | --- |
| `backend_login_refused` | ERROR | `backend`, `host`, `port`, `count`, `retry_in_secs`, `error` | The backend rejected `AUTHINFO` as bad credentials (`481`). New connections pause for `retry_in_secs` (`0` means the refusal predates a later successful login and nothing pauses) | Check the backend's username and password and the account status with the provider |
| `backend_connection_limit` | WARN | `backend`, `host`, `port`, `count`, `retry_in_secs`, `error` | The account or address is at the provider's connection limit: a `482`, or a refusal at the greeting, either `AUTHINFO` step, or `MODE READER` whose words name a connection limit. Only this refusal lowers an adaptive `connection_cap` | Lower `max_connections` for the backend, or stop other clients sharing the account |
| `backend_unavailable` | WARN | `backend`, `host`, `port`, `count`, `retry_in_secs`, `error` | The backend refused service before login: any other non-2xx greeting, such as `400` (service unavailable) or `502`. `error` carries the reply | Usually transient: alert when it persists, then check the provider's status and how many connections share the account or address |
| `backend_login_rejected` | WARN | `backend`, `host`, `port`, `count`, `retry_in_secs`, `error` | `AUTHINFO` was refused with something other than `481` or a connection limit, or got an unexpected reply. `error` carries the reply, which says why (a locked account, a rate limit, a quota) | Read the reply in `error`; check the account with the provider if it persists |
| `backend_reader_mode_refused` | ERROR | `backend`, `host`, `port`, `count`, `retry_in_secs`, `error` | Login succeeded but the backend refused `MODE READER`: the account or address may not read from this server | Check the account's reading permissions with the provider, and that the address is allowed to read |
| `backend_unreachable` | WARN | `backend`, `host`, `port`, `count`, `error` | DNS lookup, TCP connect, TLS handshake, or session setup failed or timed out | Alert when it persists: check DNS, network path, firewall, and the backend's status |
| `backend_misconfigured` | ERROR | `backend`, `host`, `port`, `count`, `error` | The backend needs something the config does not provide, such as a password or wire compression | Fix the backend's `[[servers]]` entry |
| `backend_health_check_failed` | WARN | `backend`, `checked`, `failed`, `count` | A periodic health-check cycle found failing idle connections and discarded them. `count` is failing cycles. Cleared line: `failing_cycles`, `duration_secs` | Alert when it persists; usually the backend or the path to it is degraded |
| `pool_size_reduced` | WARN | `backend`, `removed`, `new_max_size`, `connection_cap`, `active_cooldowns`, `cooldown_secs` | Connections were removed after errors and the pool shrinks for the replacement cooldown. `removed` counts removals since the previous line. Cleared line (`pool_size_reduced_cleared`): `removals`, `duration_secs`, `max_size` | Look for the backend errors that preceded it |
| `connection_cap_lowered` | WARN | `pool`, `from`, `to`, `configured` | With `connection_cap = "adaptive"`, a connection-limit refusal lowered the backend's connection cap to the connections the provider had accepted. Climbs back are logged at `info` without an event | Persistent: lower `max_connections` or `prewarm_connections` to what the account allows, or stop other clients sharing it |
| `pool_checkout_failed` | WARN | `backend`, `count`, `max_size`, `available`, `waiting`, `error` | A checkout failed inside the connection pool rather than while setting up a connection: a recycle timeout, no async runtime, or a post-create hook. No cleared line; a burst ends after a quiet minute | Should not happen in normal operation; report it with the `error` text |
| `buffer_pool_exhausted` | WARN | `pool` (`regular` or `capture`), `fallbacks_since_last_warning`, `max_pool_size`, `buffer_size` (regular) or `capacity` (capture) | Every pooled buffer was in use and fallback buffers were allocated. Logged on the first fallback, then at most once a minute with the count since the previous line. No cleared line | Raise `[memory] buffer_pool_count` or `capture_pool_count` if it recurs |
| `stats_persist_failed` | ERROR | `path`, `count`, `error` | Writing the stats file failed; metrics will not survive a restart. Cleared line: `failures`, `duration_secs` | Check free space and permissions at `path` |
| `availability_persist_failed` | ERROR | `path`, `count`, `error` | Writing the availability index failed; cached `430` facts will not survive a restart. Cleared line: `failures`, `duration_secs` | Check free space and permissions at `path` |
| `config_key_ignored` | WARN | `key`, `config` (in the message, see below) | Startup found an unknown key in `[[servers]]`, `[routing]`, `[metrics]`, or `[logging]`, usually a typo, and ignored it. Logged once | Fix or remove the key |
| `config_setting_inactive` | WARN | `setting` | A configured setting has no effect in this build or mode. Logged once | Remove the setting or enable what it depends on |
| `config_value_discouraged` | WARN | message names the value | A value is accepted but likely harmful, such as a very short or long `connection_keepalive`, or fewer buffers than backend connections. Logged once | Review the value |
| `tls_verification_disabled` | WARN | none | A backend has `tls_verify_cert = false` | Enable verification outside of testing |

Events logged while the config file is loading are held until logging is set
up and then replayed with target `nntp_proxy::startup`. A replayed line keeps
`event` as a field; its other fields are appended to `message` as
`key="value"` text.

Busy backends are summarised without an `event` field: one `INFO` line per
backend every 10 seconds at most, "Backend busy; requests skipped it or timed
out waiting for a connection", with `backlog_skips` and `busy_timeouts`
(bounded routing checkouts). Checkouts without their own limit that wait the
pool's full default wait are summarised the same way, at most once per backend
every 10 seconds, as "Backend pool full; checkouts timed out waiting for a
connection" with `pool_wait_timeouts`; they do not count toward
`busy_timeouts` and have their own `pool_wait_timeouts` counter in the stats
file.

## Periodic backend summary

`[logging] backend_summary_interval_secs` (default `0`, off; `300` is a
reasonable value; at most `86400`) logs one `INFO` line per backend with what changed since the
previous summary. It reads counters the proxy keeps anyway, so it adds no work
to request handling.

| Field | Meaning |
| --- | --- |
| `event` | `backend_summary` |
| `backend` | Backend name |
| `interval_secs` | Seconds covered by the line |
| `commands` | Commands sent to the backend |
| `articles` | `ARTICLE`/`BODY`/`HEAD` payloads received |
| `missing` | Authoritative `430` (article missing) replies, the same counter as `nntp_proxy_backend_article_missing_total` |
| `missing_pct` | `missing` as a percentage of `commands`, one decimal, clamped to `100` |
| `errors` | Errors other than `430`s: other 4xx, 5xx, and failed exchanges |
| `rx_bytes_per_sec` | Average bytes per second received from the backend |

## Example alert queries

These assume JSON output, so fields are indexed, and a stream label such as
`app="nntp-proxy"` set by your log shipper. Adjust the label and windows to
your setup.

A persistent condition logs about once a minute, so "at least 5 lines in
10 minutes" means it has lasted roughly 5 minutes or more.

### LogsQL (VictoriaLogs)

Every error:

```logsql
_time:5m level:ERROR
```

Credentials rejected by any backend:

```logsql
_time:5m event:backend_login_refused | stats by (backend) count() as lines
```

A backend unreachable for about 5 minutes:

```logsql
_time:10m event:backend_unreachable
  | stats by (backend) count() as lines, sum(count) as failures
  | filter lines:>=5
```

Persistence failures:

```logsql
_time:10m event:in(stats_persist_failed, availability_persist_failed)
```

A backend answering more than half of its commands with article-missing
replies (needs `backend_summary_interval_secs`):

```logsql
_time:15m event:backend_summary missing_pct:>50
```

With text output, the event value is still a whole word, so a plain word filter
such as `_time:5m backend_unreachable` finds the same lines.

### LogQL (Loki)

Every error:

```logql
{app="nntp-proxy"} | json | level="ERROR"
```

Credentials rejected, as an alert expression:

```logql
sum by (backend) (count_over_time({app="nntp-proxy"} | json | event="backend_login_refused" [5m])) > 0
```

A backend unreachable for about 5 minutes:

```logql
sum by (backend) (count_over_time({app="nntp-proxy"} | json | event="backend_unreachable" [10m])) >= 5
```

Total failures behind those lines:

```logql
sum by (backend) (sum_over_time({app="nntp-proxy"} | json | event="backend_unreachable" | unwrap count [10m]))
```

Persistence failures:

```logql
count_over_time({app="nntp-proxy"} | json | event=~"stats_persist_failed|availability_persist_failed" [10m]) > 0
```

## Choosing what is logged

`RUST_LOG` sets the filter, as for any `tracing` program. The default is
`info`. Examples:

```sh
RUST_LOG=warn nntp-proxy --config config.toml
RUST_LOG=info,nntp_proxy::pool=debug nntp-proxy --config config.toml
```
