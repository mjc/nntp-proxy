use std::collections::HashMap;
use std::io::{self, BufRead};

fn main() {
    let stdin = io::stdin();
    let mut samples: Vec<Sample> = Vec::new();
    let mut current_comm = String::new();
    let mut current_tid: u32 = 0;
    let mut current_time: f64 = 0.0;
    let mut current_stack: Vec<String> = Vec::new();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.is_empty() {
            // End of stack trace — flush
            if !current_comm.is_empty() && !current_stack.is_empty() {
                samples.push(Sample {
                    comm: current_comm.clone(),
                    tid: current_tid,
                    time: current_time,
                    stack: std::mem::take(&mut current_stack),
                });
            }
            current_comm.clear();
            current_stack.clear();
            continue;
        }

        if !line.starts_with('\t') && !line.starts_with(' ') {
            // Header line: "comm tid [cpu] time: event:"
            // e.g. "nntp-proxy-tui 12345 [003] 12345.678901: cpu-clock:"
            if let Some(parsed) = parse_header(&line) {
                current_comm = parsed.0;
                current_tid = parsed.1;
                current_time = parsed.2;
                current_stack.clear();
            }
        } else {
            // Stack frame line: "\t     addr func+offset (module)"
            let trimmed = line.trim();
            if let Some(func) = parse_frame(trimmed) {
                current_stack.push(func);
            }
        }
    }

    // Flush last sample
    if !current_comm.is_empty() && !current_stack.is_empty() {
        samples.push(Sample {
            comm: current_comm,
            tid: current_tid,
            time: current_time,
            stack: current_stack,
        });
    }

    if samples.is_empty() {
        eprintln!("No samples parsed. Make sure to pipe `perf script` output.");
        std::process::exit(1);
    }

    print_thread_breakdown(&samples);
    println!();
    print_top_functions(&samples, 40);
    println!();
    print_top_functions_per_thread(&samples, 15);
    println!();
    print_callee_edges(&samples, 30);
    println!();
    print_timeline(&samples, 10);
    println!();
    print_category_summary(&samples);
}

struct Sample {
    comm: String,
    tid: u32,
    time: f64,
    stack: Vec<String>,
}

fn parse_header(line: &str) -> Option<(String, u32, f64)> {
    // Two common formats:
    //   "comm pid/tid [cpu] timestamp: count event:"   (with CPU field)
    //   "comm tid timestamp: count event:"             (without CPU field)
    // The comm may contain spaces, so we search for patterns carefully.

    // Strategy: find the first colon — everything before it ends with the timestamp.
    // The timestamp is a float like "1408261.500056".
    let first_colon = line.find(':')?;
    let before_colon = line[..first_colon].trim_end();

    // The timestamp is the last whitespace-delimited token before the colon
    let ts_space = before_colon.rfind(' ')?;
    let ts_str = &before_colon[ts_space + 1..];
    let time: f64 = ts_str.parse().ok()?;

    // Everything before the timestamp is "comm tid [cpu]" or "comm tid"
    let prefix = before_colon[..ts_space].trim_end();

    // Strip optional [cpu] field
    let prefix = if prefix.ends_with(']') {
        let bracket = prefix.rfind('[')?;
        prefix[..bracket].trim_end()
    } else {
        prefix
    };

    // Last token is tid or pid/tid
    let last_space = prefix.rfind(' ')?;
    let comm = prefix[..last_space].trim().to_string();
    let tid_str = &prefix[last_space + 1..];

    let tid: u32 = if let Some(slash) = tid_str.find('/') {
        tid_str[slash + 1..].parse().ok()?
    } else {
        tid_str.parse().ok()?
    };

    Some((comm, tid, time))
}

fn parse_frame(line: &str) -> Option<String> {
    // Format: "addr func+0xoffset (module)" or "addr func (module)" or "addr [unknown]"
    // We want just the function name.
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let rest = parts[1].trim();

    // Strip module suffix " (/path/to/lib)" or " ([module])"
    let func_part = if let Some(paren) = rest.rfind(" (") {
        &rest[..paren]
    } else {
        rest
    };

    // Strip "+0xoffset" suffix
    let func = if let Some(plus) = func_part.rfind('+') {
        // Verify it's +0x... pattern
        let after = &func_part[plus + 1..];
        if after.starts_with("0x") || after.chars().all(|c| c.is_ascii_hexdigit()) {
            &func_part[..plus]
        } else {
            func_part
        }
    } else {
        func_part
    };

    if func.is_empty() {
        return None;
    }

    Some(func.to_string())
}

// ── Reports ──────────────────────────────────────────────────────────

fn print_thread_breakdown(samples: &[Sample]) {
    let total = samples.len();
    let mut by_thread: HashMap<(u32, &str), usize> = HashMap::new();

    for s in samples {
        *by_thread.entry((s.tid, &s.comm)).or_insert(0) += 1;
    }

    let mut threads: Vec<_> = by_thread.into_iter().collect();
    threads.sort_by(|a, b| b.1.cmp(&a.1));

    println!("═══ Thread Breakdown ({} total samples) ═══\n", total);
    println!("{:>7} {:>10}  {:>7}  {}", "%", "samples", "tid", "comm");
    println!("{}", "-".repeat(60));

    for ((tid, comm), count) in &threads {
        let pct = *count as f64 / total as f64 * 100.0;
        println!("{:>6.2}% {:>10}  {:>7}  {}", pct, count, tid, comm);
    }
}

fn print_top_functions(samples: &[Sample], n: usize) {
    let total = samples.len();
    // "self" time = only the leaf frame (top of stack = index 0)
    let mut leaf_counts: HashMap<&str, usize> = HashMap::new();

    for s in samples {
        if let Some(leaf) = s.stack.first() {
            *leaf_counts.entry(leaf).or_insert(0) += 1;
        }
    }

    let mut funcs: Vec<_> = leaf_counts.into_iter().collect();
    funcs.sort_by(|a, b| b.1.cmp(&a.1));

    println!("═══ Top {} Functions (self/on-CPU time) ═══\n", n);
    println!("{:>7} {:>10}  {}", "%", "samples", "Function");
    println!("{}", "-".repeat(100));

    let mut shown_pct = 0.0;
    for (func, count) in funcs.iter().take(n) {
        let pct = *count as f64 / total as f64 * 100.0;
        println!("{:>6.2}% {:>10}  {}", pct, count, truncate(func, 80));
        shown_pct += pct;
    }
    println!("{}", "-".repeat(100));
    println!("{:>6.2}%             Total ({} functions shown)", shown_pct, funcs.len().min(n));
}

fn print_top_functions_per_thread(samples: &[Sample], n: usize) {
    // Group by thread
    let mut by_thread: HashMap<(u32, &str), Vec<&Sample>> = HashMap::new();
    for s in samples {
        by_thread.entry((s.tid, &s.comm)).or_default().push(s);
    }

    let mut threads: Vec<_> = by_thread.into_iter().collect();
    threads.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    println!("═══ Top Functions Per Thread ═══");

    for ((tid, comm), thread_samples) in threads.iter().take(8) {
        let thread_total = thread_samples.len();
        let mut leaf_counts: HashMap<&str, usize> = HashMap::new();
        for s in thread_samples {
            if let Some(leaf) = s.stack.first() {
                *leaf_counts.entry(leaf).or_insert(0) += 1;
            }
        }

        let mut funcs: Vec<_> = leaf_counts.into_iter().collect();
        funcs.sort_by(|a, b| b.1.cmp(&a.1));

        println!("\n─── {} (tid {}, {} samples) ───\n", comm, tid, thread_total);
        println!("{:>7} {:>10}  {}", "%", "samples", "Function");

        for (func, count) in funcs.iter().take(n) {
            let pct = *count as f64 / thread_total as f64 * 100.0;
            println!("{:>6.2}% {:>10}  {}", pct, count, truncate(func, 75));
        }
    }
}

fn print_callee_edges(samples: &[Sample], n: usize) {
    // Build caller -> callee edges from stack traces
    // Stack is bottom-up: [leaf, ..., root] from perf script default
    // Actually perf script outputs top-down by default: first frame = leaf
    // So stack[0] = leaf (callee), stack[1] = caller, etc.
    let mut edges: HashMap<(&str, &str), usize> = HashMap::new();

    for s in samples {
        for w in s.stack.windows(2) {
            let callee = &w[0];
            let caller = &w[1];
            *edges.entry((caller, callee)).or_insert(0) += 1;
        }
    }

    let mut edge_list: Vec<_> = edges.into_iter().collect();
    edge_list.sort_by(|a, b| b.1.cmp(&a.1));

    let total = samples.len();
    println!("═══ Top {} Caller → Callee Edges ═══\n", n);
    println!("{:>7} {:>10}  {} → {}", "%", "samples", "Caller", "Callee");
    println!("{}", "-".repeat(120));

    for ((caller, callee), count) in edge_list.iter().take(n) {
        let pct = *count as f64 / total as f64 * 100.0;
        println!(
            "{:>6.2}% {:>10}  {} → {}",
            pct,
            count,
            truncate(caller, 50),
            truncate(callee, 50)
        );
    }
}

fn print_timeline(samples: &[Sample], buckets: usize) {
    if samples.is_empty() {
        return;
    }

    let min_time = samples.iter().map(|s| s.time).fold(f64::INFINITY, f64::min);
    let max_time = samples.iter().map(|s| s.time).fold(f64::NEG_INFINITY, f64::max);
    let duration = max_time - min_time;

    if duration <= 0.0 {
        println!("═══ Timeline ═══\n");
        println!("All samples at same timestamp — cannot bucket.");
        return;
    }

    let bucket_width = duration / buckets as f64;

    // Per-bucket: total count, and per-thread counts
    struct Bucket {
        start: f64,
        total: usize,
        by_thread: HashMap<u32, usize>,
        top_funcs: HashMap<String, usize>,
    }

    let mut bucket_vec: Vec<Bucket> = (0..buckets)
        .map(|i| {
            let start = min_time + i as f64 * bucket_width;
            let _end = if i == buckets - 1 { max_time + 0.001 } else { start + bucket_width };
            Bucket {
                start,
                total: 0,
                by_thread: HashMap::new(),
                top_funcs: HashMap::new(),
            }
        })
        .collect();

    for s in samples {
        let idx = ((s.time - min_time) / bucket_width) as usize;
        let idx = idx.min(buckets - 1);
        let b = &mut bucket_vec[idx];
        b.total += 1;
        *b.by_thread.entry(s.tid).or_insert(0) += 1;
        if let Some(leaf) = s.stack.first() {
            *b.top_funcs.entry(leaf.clone()).or_insert(0) += 1;
        }
    }

    // Find top threads overall for column headers
    let mut thread_totals: HashMap<u32, usize> = HashMap::new();
    for s in samples {
        *thread_totals.entry(s.tid).or_insert(0) += 1;
    }
    let mut top_threads: Vec<_> = thread_totals.into_iter().collect();
    top_threads.sort_by(|a, b| b.1.cmp(&a.1));
    let top_threads: Vec<u32> = top_threads.iter().take(6).map(|t| t.0).collect();

    // Thread name lookup
    let mut tid_to_comm: HashMap<u32, &str> = HashMap::new();
    for s in samples {
        tid_to_comm.entry(s.tid).or_insert(&s.comm);
    }

    println!("═══ Timeline ({:.1}s duration, {} buckets) ═══\n", duration, buckets);
    println!(
        "This shows sample distribution over time to distinguish cold (early) vs hot (late) phases.\n"
    );

    // Header
    print!("{:>12} {:>8}", "Time(s)", "Samples");
    for tid in &top_threads {
        let name = tid_to_comm.get(tid).unwrap_or(&"?");
        let label = if name.len() > 10 {
            &name[..10]
        } else {
            name
        };
        print!("  {:>10}", label);
    }
    println!("  Top function");
    println!("{}", "-".repeat(120));

    for b in &bucket_vec {
        let offset = b.start - min_time;
        print!("{:>8.1}-{:<3.1} {:>8}", offset, offset + bucket_width, b.total);

        for tid in &top_threads {
            let count = b.by_thread.get(tid).copied().unwrap_or(0);
            let pct = if b.total > 0 {
                count as f64 / b.total as f64 * 100.0
            } else {
                0.0
            };
            print!("  {:>7.1}%  ", pct);
        }

        // Top function in this bucket
        if let Some((func, _count)) = b.top_funcs.iter().max_by_key(|(_k, v)| *v) {
            print!("  {}", truncate(func, 40));
        }

        println!();
    }

    // Compare first half vs second half
    let mid = buckets / 2;
    let first_half: usize = bucket_vec[..mid].iter().map(|b| b.total).sum();
    let second_half: usize = bucket_vec[mid..].iter().map(|b| b.total).sum();

    println!();
    println!(
        "First half: {} samples ({:.1}%), Second half: {} samples ({:.1}%)",
        first_half,
        first_half as f64 / samples.len() as f64 * 100.0,
        second_half,
        second_half as f64 / samples.len() as f64 * 100.0,
    );

    // Show top function differences between halves
    let mut first_funcs: HashMap<&str, usize> = HashMap::new();
    let mut second_funcs: HashMap<&str, usize> = HashMap::new();

    for s in samples {
        let idx = ((s.time - min_time) / bucket_width) as usize;
        let idx = idx.min(buckets - 1);
        if let Some(leaf) = s.stack.first() {
            if idx < mid {
                *first_funcs.entry(leaf).or_insert(0) += 1;
            } else {
                *second_funcs.entry(leaf).or_insert(0) += 1;
            }
        }
    }

    println!("\nFunctions hotter in FIRST half (cold phase):");
    print_phase_diff(&first_funcs, &second_funcs, first_half, second_half, 10);

    println!("\nFunctions hotter in SECOND half (hot/cached phase):");
    print_phase_diff(&second_funcs, &first_funcs, second_half, first_half, 10);
}

fn print_phase_diff(
    primary: &HashMap<&str, usize>,
    other: &HashMap<&str, usize>,
    primary_total: usize,
    other_total: usize,
    n: usize,
) {
    if primary_total == 0 || other_total == 0 {
        println!("  (insufficient data)");
        return;
    }

    let mut diffs: Vec<(&str, f64, f64, f64)> = Vec::new();
    for (func, &count) in primary {
        let pct_primary = count as f64 / primary_total as f64 * 100.0;
        let pct_other = other.get(func).copied().unwrap_or(0) as f64 / other_total as f64 * 100.0;
        let diff = pct_primary - pct_other;
        if diff > 0.1 {
            diffs.push((func, pct_primary, pct_other, diff));
        }
    }
    diffs.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    println!("{:>7} {:>7} {:>7}  {}", "this%", "other%", "diff%", "Function");
    for (func, pct_p, pct_o, diff) in diffs.iter().take(n) {
        println!(
            "{:>6.2}% {:>6.2}% {:>+6.2}%  {}",
            pct_p,
            pct_o,
            diff,
            truncate(func, 70)
        );
    }
}

fn print_category_summary(samples: &[Sample]) {
    let total = samples.len();
    let mut categories: HashMap<&str, usize> = HashMap::new();

    for s in samples {
        if let Some(leaf) = s.stack.first() {
            let cat = categorize(leaf);
            *categories.entry(cat).or_insert(0) += 1;
        }
    }

    let mut cats: Vec<_> = categories.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1));

    println!("═══ Category Summary (self time) ═══\n");
    println!("{:>7} {:>10}  {}", "%", "samples", "Category");
    println!("{}", "-".repeat(40));

    for (cat, count) in &cats {
        let pct = *count as f64 / total as f64 * 100.0;
        println!("{:>6.2}% {:>10}  {}", pct, count, cat);
    }
}

fn categorize(name: &str) -> &'static str {
    let lower = name.to_lowercase();

    if lower.contains("foyer") || lower.contains("hybrid_cache") || lower.contains("hybridarticle")
        || lower.contains("article_cache") || lower.contains("unified_cache")
        || lower.contains("cache::") || lower.contains("moka")
    {
        return "Cache/Foyer";
    }

    if lower.contains("nntp") || lower.contains("precheck") || lower.contains("article_routing")
        || lower.contains("client_session") || lower.contains("backend_execution")
        || lower.contains("command_guard") || lower.contains("route_command")
        || lower.contains("status_code") || lower.contains("message_id")
    {
        return "NNTP Protocol";
    }

    if lower.contains("tls") || lower.contains("ssl") || lower.contains("rustls")
        || lower.contains("aes") || lower.contains("cipher") || lower.contains("encrypt")
        || lower.contains("decrypt") || lower.contains("handshake") || lower.contains("aws_lc")
        || lower.contains("ring::") || lower.contains("chacha")
    {
        return "TLS/Crypto";
    }

    if lower.contains("lz4") || lower.contains("compress") || lower.contains("decompress")
        || lower.contains("zstd")
    {
        return "Compression";
    }

    if lower.contains("deadpool") || lower.contains("pool") || lower.contains("connection_provider")
    {
        return "Connection Pool";
    }

    if lower.contains("recv") || lower.contains("send") || lower.contains("tcp")
        || lower.contains("socket") || lower.contains("inet") || lower.contains("skb")
        || lower.contains("net_")
    {
        return "Network I/O";
    }

    if lower.contains("zfs") || lower.contains("zpl") || lower.contains("zil")
        || lower.contains("vfs") || lower.contains("write_all") || lower.contains("ext4")
        || lower.contains("xfs") || lower.contains("btrfs") || lower.contains("block_")
        || lower.contains("io_uring") || lower.contains("pread") || lower.contains("pwrite")
    {
        return "Disk I/O";
    }

    if lower.contains("futex") || lower.contains("mutex") || lower.contains("lock")
        || lower.contains("rwlock") || lower.contains("semaphore") || lower.contains("parking_lot")
    {
        return "Locks/Futex";
    }

    if lower.contains("epoll") || lower.contains("poll") || lower.contains("mio") {
        return "Event Loop";
    }

    if lower.contains("tokio") || lower.contains("runtime") {
        return "Tokio Runtime";
    }

    if lower.contains("futures") || lower.contains("async") || lower.contains("waker") {
        return "Async/Futures";
    }

    if lower.contains("schedule") || lower.contains("switch") || lower.contains("context") {
        return "Scheduling";
    }

    if lower.contains("alloc") || lower.contains("malloc") || lower.contains("free")
        || lower.contains("mmap") || lower.contains("brk") || lower.contains("jemalloc")
    {
        return "Memory";
    }

    if name.starts_with("__x64_sys_") || name.starts_with("syscall")
        || name.starts_with("do_syscall") || name.starts_with("entry_SYSCALL")
    {
        return "Syscall";
    }

    "Other"
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
