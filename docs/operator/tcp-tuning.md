# TCP Tuning

This guide separates two things:

1. Socket options the proxy actually applies today
2. Optional host-level tuning that may help some deployments

The first part tracks current implementation behavior. The OS-level tuning sections are general guidance, not project-benchmarked guarantees.

## Socket options applied by the proxy

The proxy does not apply one uniform set of TCP options to every socket. Client connections and pooled backend connections are configured differently.

### Client connections

Accepted client sockets go through `src/network.rs` and get:

- configurable receive/send buffer sizes via `[memory].socket_recv_buffer_size` and `[memory].socket_send_buffer_size`
- `SO_LINGER=5s`
- `TCP_NODELAY`
- on Linux, best-effort:
  - `TCP_USER_TIMEOUT=30s`
  - `IP_TOS=0x08` (`IPTOS_THROUGHPUT`)

Linux-specific options are best-effort. The proxy logs a debug message if the OS rejects them, but it does not fail startup or the client connection.

### Pooled backend connections

Backend sockets created in `src/pool/deadpool_connection.rs` get:

- configurable receive/send buffer sizes before connect
- `SO_REUSEADDR` before connect
- socket-level TCP keepalive after connect
- `TCP_NODELAY` after connect

This is separate from the optional NNTP-level `connection_keepalive` setting, which sends `DATE` on idle pooled backend connections.

## OS-level tuning (optional)

These changes are deployment-specific and usually require root or administrator access.

### Linux

#### Consider BBR for remote backend links

BBR can help when the proxy talks to remote NNTP backends over the internet. It is unlikely to matter for localhost traffic and may not matter much on a fast LAN.

```bash
sysctl net.ipv4.tcp_congestion_control
echo "net.core.default_qdisc=fq" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_congestion_control=bbr" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p
```

#### Raise OS socket-buffer limits if your configured buffers are being clamped

```bash
sysctl net.core.rmem_max
sysctl net.core.wmem_max
echo "net.core.rmem_max=33554432" | sudo tee -a /etc/sysctl.conf
echo "net.core.wmem_max=33554432" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_rmem=4096 87380 33554432" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_wmem=4096 65536 33554432" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p
```

#### Check that window scaling is enabled

```bash
sysctl net.ipv4.tcp_window_scaling
```

### Windows

Windows autotuning is usually reasonable already:

```powershell
netsh interface tcp show global
netsh interface tcp set global autotuninglevel=normal
```

### macOS

If you change default socket-buffer limits, treat that as host-specific tuning:

```bash
sudo sysctl -w net.inet.tcp.sendspace=2097152
sudo sysctl -w net.inet.tcp.recvspace=2097152
```

## Verification

On Linux, inspect live sockets instead of assuming requested buffer sizes were applied unchanged:

```bash
ss -tmi
ss -ti
```

If you are tuning remote backend performance, focus on proxy-to-backend connections rather than only the local client-to-proxy leg.

## References

- [Linux TCP Tuning](https://fasterdata.es.net/network-tuning/linux/)
- [BBR Congestion Control](https://github.com/google/bbr)
- [socket2 crate documentation](https://docs.rs/socket2/)
- [TCP_USER_TIMEOUT RFC 5482](https://datatracker.ietf.org/doc/html/rfc5482)
