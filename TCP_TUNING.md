# TCP Tuning Guide for High-Performance NNTP Proxy

This document describes TCP optimizations for maximizing throughput and minimizing latency in the NNTP proxy.

## Application-Level Optimizations (Built-in)

The proxy automatically applies the following TCP optimizations:

### Cross-Platform (Linux, macOS, Windows)
- **Large socket buffers**: 16MB send/receive for high-throughput connections, 4MB for pooled connections
- **SO_LINGER**: 5-second timeout to prevent indefinite blocking on socket close
- **TCP keepalive**: Probes after 60s idle, retries every 10s
- **TCP_NODELAY**: Enabled on pooled backend connections for lower latency
- **SO_REUSEADDR**: Allows quick socket reuse after connection close

### Linux-Specific
- **TCP_USER_TIMEOUT**: 30-second timeout for faster dead connection detection (vs default ~15 minutes)
- **IP_TOS**: Marks packets for throughput optimization (0x08 = IPTOS_THROUGHPUT)

## System-Level Tuning (Optional but Recommended)

These require root/admin privileges and system configuration changes.

### Linux

#### 1. TCP Congestion Control - BBR (Highly Recommended for Backend Connections)

**Note:** This only affects proxy ↔ backend server connections over the internet. Localhost client connections don't use congestion control at all (loopback has no congestion).

BBR (Bottleneck Bandwidth and RTT) achieves significantly higher throughput than the default CUBIC algorithm, especially on high-bandwidth, long-distance connections to NNTP backend servers.

```bash
# Check current congestion control algorithm
sysctl net.ipv4.tcp_congestion_control

# Enable BBR (requires Linux 4.9+)
# This affects all TCP connections, including proxy → backend
echo "net.core.default_qdisc=fq" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_congestion_control=bbr" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# Verify
sysctl net.ipv4.tcp_congestion_control
# Should show: net.ipv4.tcp_congestion_control = bbr
```

**When BBR helps:**
- ✅ Proxy running on cloud server connecting to remote NNTP servers
- ✅ High-latency links (>20ms RTT)
- ✅ Connections with packet loss or variable bandwidth

**When BBR doesn't matter:**
- ❌ Client → Proxy (localhost) - no congestion control used
- ❌ LAN connections (minimal congestion)
- ❌ Very low latency links (<5ms RTT)

#### 2. Increase Maximum Socket Buffer Sizes

The proxy requests 16MB socket buffers, but if the system maximum is lower, it will be clamped. Raise the system limits:

```bash
# Current maximums (often 4MB or less by default)
sysctl net.core.rmem_max
sysctl net.core.wmem_max

# Set to 32MB to allow room for 16MB application buffers
echo "net.core.rmem_max=33554432" | sudo tee -a /etc/sysctl.conf
echo "net.core.wmem_max=33554432" | sudo tee -a /etc/sysctl.conf

# TCP auto-tuning ranges: min, default, max (in bytes)
echo "net.ipv4.tcp_rmem=4096 87380 33554432" | sudo tee -a /etc/sysctl.conf
echo "net.ipv4.tcp_wmem=4096 65536 33554432" | sudo tee -a /etc/sysctl.conf

# Apply
sudo sysctl -p
```

#### 3. Enable TCP Window Scaling (Usually Already Enabled)

Required for window sizes >64KB. Verify it's enabled:

```bash
sysctl net.ipv4.tcp_window_scaling
# Should be: net.ipv4.tcp_window_scaling = 1
```

#### 4. Additional Performance Tweaks

```bash
# Increase max backlog for incoming connections
echo "net.core.netdev_max_backlog=5000" | sudo tee -a /etc/sysctl.conf

# Increase max orphaned TCP connections
echo "net.ipv4.tcp_max_orphans=262144" | sudo tee -a /etc/sysctl.conf

# Faster TIME_WAIT socket recycling (be careful with this)
echo "net.ipv4.tcp_tw_reuse=1" | sudo tee -a /etc/sysctl.conf

# Apply all
sudo sysctl -p
```

### Windows

#### 1. Increase TCP Window Size

Windows auto-tuning is usually good, but you can optimize:

```powershell
# Check current auto-tuning level (should be "normal")
netsh interface tcp show global

# Set to experimental for maximum throughput (use with caution)
netsh interface tcp set global autotuninglevel=experimental

# Or stick with normal (recommended)
netsh interface tcp set global autotuninglevel=normal

# Enable window scaling
netsh interface tcp set global chimney=enabled
netsh interface tcp set global rss=enabled
netsh interface tcp set global netdma=enabled
```

#### 2. Disable Nagle's Algorithm System-Wide (Optional)

The proxy already sets TCP_NODELAY per-socket, but you can disable Nagle globally:

```powershell
# Requires admin PowerShell
reg add "HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" /v TcpAckFrequency /t REG_DWORD /d 1 /f
reg add "HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" /v TCPNoDelay /t REG_DWORD /d 1 /f

# Restart required
```

### macOS

macOS has good defaults for most scenarios. Optional tweaks:

```bash
# Increase socket buffer sizes
sudo sysctl -w net.inet.tcp.sendspace=2097152
sudo sysctl -w net.inet.tcp.recvspace=2097152

# Make permanent by adding to /etc/sysctl.conf:
echo "net.inet.tcp.sendspace=2097152" | sudo tee -a /etc/sysctl.conf
echo "net.inet.tcp.recvspace=2097152" | sudo tee -a /etc/sysctl.conf
```

## Verification

### Check Applied Settings

```bash
# Linux: View actual socket buffer sizes of a running connection
ss -tmi | grep -A 10 ":8119"

# Or with netstat
netstat -tnp | grep nntp-proxy

# Check if BBR is active
ss -ti | grep bbr
```

### Benchmark

Test throughput before and after tuning:

```bash
# Download a large article and measure speed
time echo "ARTICLE <message-id>" | nc localhost 8119 > /dev/null

# Or use a proper NNTP client with speed measurement
```

## Expected Performance Impact

**Important:** These optimizations primarily affect **proxy ↔ backend** connections. Client ↔ proxy (localhost) performance is already optimal due to loopback's inherent characteristics (no congestion, memory-speed bandwidth).

| Optimization | Throughput Gain | Latency Improvement | Applies To | Complexity |
|--------------|----------------|---------------------|------------|------------|
| BBR congestion control | +20-40% | Moderate | Backend only | Low |
| 16MB socket buffers | +10-30% | Minimal | Both | None (automatic) |
| TCP_USER_TIMEOUT | 0% | High (failure detection) | Backend only | None (automatic) |
| SO_LINGER | 0% | High (clean shutdown) | Both | None (automatic) |
| Window scaling | Essential for >64KB windows | Minimal | Backend only | Usually enabled |

## Troubleshooting

### "Cannot allocate memory" errors

Reduce buffer sizes in `src/constants.rs`:
```rust
pub const HIGH_THROUGHPUT_RECV_BUFFER: usize = 8 * 1024 * 1024;  // 8MB instead of 16MB
pub const HIGH_THROUGHPUT_SEND_BUFFER: usize = 8 * 1024 * 1024;
```

### Windows: "An invalid argument was supplied"

Windows has different limits. The proxy will gracefully fall back if buffer allocation fails.

### Verify sysctl changes persist after reboot

Add to `/etc/sysctl.conf` or create a file in `/etc/sysctl.d/99-nntp-proxy.conf`

## Platform-Specific Notes

### Linux
- **Best platform for maximum performance** due to BBR, TCP_USER_TIMEOUT, and extensive tuning options
- Kernel 4.9+ recommended for BBR support
- Use `ss -ti` to inspect live connection parameters

### Windows
- **Good performance with default settings** - auto-tuning works well
- Per-socket optimizations (buffers, linger) are applied automatically
- System-wide tuning options are more limited than Linux

### macOS
- **Good default behavior** for most use cases
- Limited tuning options compared to Linux
- Focus on application-level optimizations (already built-in)

## References

- [Linux TCP Tuning](https://fasterdata.es.net/network-tuning/linux/)
- [BBR Congestion Control](https://github.com/google/bbr)
- [socket2 crate documentation](https://docs.rs/socket2/)
- [TCP_USER_TIMEOUT RFC 5482](https://datatracker.ietf.org/doc/html/rfc5482)
