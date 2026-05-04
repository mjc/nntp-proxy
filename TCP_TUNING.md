# TCP Tuning Guide for High-Performance NNTP Proxy

This document separates two things that were previously mixed together:

1. **Socket options the proxy actually sets today**
2. **Optional operating-system tuning that may help some deployments**

The first section is meant to track the current implementation. The later OS-level
sections are general guidance, not project-benchmarked guarantees.

## Socket Options Applied by the Proxy

The proxy does not apply one uniform set of TCP options to every socket. Client
connections and pooled backend connections are configured a little differently.

### Client connections

Accepted client sockets go through the optimizer in `src/network.rs` and get:

- **Configurable receive/send buffer sizes** via `[memory].socket_recv_buffer_size`
  and `[memory].socket_send_buffer_size` (16 MiB defaults)
- **`SO_LINGER=5s`**
- **`TCP_NODELAY`**
- On **Linux only**, best-effort:
  - **`TCP_USER_TIMEOUT=30s`**
  - **`IP_TOS=0x08`** (`IPTOS_THROUGHPUT`)

Linux-specific options are best-effort: the proxy logs a debug message if the OS
rejects them, but it does not fail startup or the client connection.

### Pooled backend connections

Backend sockets created for the connection pool in `src/pool/deadpool_connection.rs`
get:

- **Configurable receive/send buffer sizes** before connect
- **`SO_REUSEADDR`** before connect
- **Socket-level TCP keepalive** after connect:
  - idle time: **60s**
  - interval: **10s**
- **`TCP_NODELAY`** after connect

This is distinct from the optional NNTP-level `connection_keepalive` setting in
the config, which controls periodic `DATE` health probes on idle pooled backend
connections.

## OS-Level Tuning (Optional)

These changes require root or administrator access and are deployment-specific.
They are not validated by the repository test suite, and the throughput gains
depend heavily on WAN latency, packet loss, backend behavior, and host limits.

### Linux

#### 1. Consider BBR for remote backend links

BBR can help when the proxy talks to **remote** NNTP backends over the internet,
especially on higher-latency or lossy links. It is unlikely to matter for
localhost traffic and may not matter much on a fast LAN.

```bash
# Check current congestion control algorithm
sysctl net.ipv4.tcp_congestion_control

# Enable BBR (requires Linux 4.9+)
echo "net.core.default_qdisc=fq" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_congestion_control=bbr" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p
```

Use this only if you are comfortable making a host-wide TCP policy change.

#### 2. Raise OS socket-buffer limits if your configured buffers are being clamped

The proxy requests the configured socket buffer sizes, but the OS can clamp those
requests to lower maxima.

```bash
# Inspect current maxima
sysctl net.core.rmem_max
sysctl net.core.wmem_max

# Example: allow room for 16 MiB application buffers
echo "net.core.rmem_max=33554432" | sudo tee -a /etc/sysctl.conf
echo "net.core.wmem_max=33554432" | sudo tee -a /etc/sysctl.conf

# TCP autotuning ranges: min, default, max
echo "net.ipv4.tcp_rmem=4096 87380 33554432" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_wmem=4096 65536 33554432" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p
```

If you do not need 16 MiB buffers, lowering the application config is usually
safer than raising global sysctl limits.

#### 3. Check that window scaling is enabled

Required for buffers larger than 64 KiB.

```bash
sysctl net.ipv4.tcp_window_scaling
```

#### 4. Other sysctl tweaks

Settings like `net.core.netdev_max_backlog`, `tcp_max_orphans`, or
`tcp_tw_reuse` are generic Linux tuning knobs. They may help some installations,
but this project does not depend on them and does not benchmark them directly.
Treat them as host-level tuning, not proxy-specific requirements.

### Windows

Windows autotuning is usually reasonable already. If you tune it, treat the
changes as general Windows TCP policy rather than something specific to this
proxy.

```powershell
netsh interface tcp show global
netsh interface tcp set global autotuninglevel=normal
```

Be cautious with older advice around `chimney`, `rss`, `netdma`, registry-wide
Nagle tweaks, or experimental autotuning modes; those are platform-level changes
with tradeoffs and are not validated here.

### macOS

macOS usually needs less manual tuning. If you change system socket-buffer
defaults, treat that as host-specific tuning and verify it with normal macOS
network tools.

```bash
sudo sysctl -w net.inet.tcp.sendspace=2097152
sudo sysctl -w net.inet.tcp.recvspace=2097152
```

## Verification

### Check applied settings on live sockets

On Linux, inspect live sockets rather than assuming the requested values were
accepted unchanged:

```bash
# Inspect listening and connected sockets
ss -tmi | grep -A 10 ":8119"

# Inspect backend flows more directly when possible
ss -ti
```

If you are tuning remote backend performance, focus on the **proxy-to-backend**
connections, not just the local client-to-proxy leg.

### Measure realistic workloads

Do not use a localhost `nc` command as proof that backend-side TCP tuning helped;
that mostly exercises the local client-to-proxy path.

Prefer:

- a real NNTP client workload through the proxy
- repeated message-ID fetches against representative remote backends
- before/after measurements of throughput, latency, retransmits, and error rates

## Expected Impact

The likely payoff depends on where the bottleneck is:

- **Remote backend links:** BBR and larger OS socket maxima may help
- **Localhost or low-latency LAN:** expect little benefit from host-level TCP tuning
- **Failure detection:** `TCP_USER_TIMEOUT`, socket keepalive, and NNTP health
  probes help more with cleanup and reuse safety than raw throughput
- **Memory pressure:** large socket buffers improve headroom for throughput but
  increase per-connection memory use

## Troubleshooting

### "Cannot allocate memory" errors

Reduce configured socket buffers before changing source code:

```toml
[memory]
socket_recv_buffer_size = 8388608
socket_send_buffer_size = 8388608
```

If RAM usage is still too high, also lower `buffer_pool_count` and
`capture_pool_count`.

### Windows: "An invalid argument was supplied"

Windows socket limits differ from Linux. Use smaller configured buffer sizes if
necessary.

### Make sysctl changes persistent

Persist Linux sysctl settings in `/etc/sysctl.conf` or a file under
`/etc/sysctl.d/`.

## References

- [Linux TCP Tuning](https://fasterdata.es.net/network-tuning/linux/)
- [BBR Congestion Control](https://github.com/google/bbr)
- [socket2 crate documentation](https://docs.rs/socket2/)
- [TCP_USER_TIMEOUT RFC 5482](https://datatracker.ietf.org/doc/html/rfc5482)
