use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;

#[allow(dead_code)]
struct Entry {
    name: String,
    samples: u64,
    percent: f64,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <flamegraph.svg> [command] [args...]", args[0]);
        eprintln!();
        eprintln!("Commands:");
        eprintln!("  top [N] [min%]     Show top N functions (default: 30, min: 1.0%)");
        eprintln!("  search <pattern>   Search for functions matching pattern");
        eprintln!("  syscalls           Show syscall breakdown");
        eprintln!("  summary            Show categorized summary");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} flamegraph.svg top 20", args[0]);
        eprintln!("  {} flamegraph.svg search foyer", args[0]);
        eprintln!("  {} flamegraph.svg syscalls", args[0]);
        eprintln!("  {} flamegraph.svg summary", args[0]);
        std::process::exit(1);
    }

    let svg_path = &args[1];
    let command = args.get(2).map(|s| s.as_str()).unwrap_or("top");

    let content = fs::read_to_string(svg_path)?;
    let entries = parse_entries(&content);

    match command {
        "top" => {
            let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            let min_pct: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.0);
            cmd_top(&entries, n, min_pct);
        }
        "search" => {
            let pattern = args.get(3).map(|s| s.as_str()).unwrap_or("");
            cmd_search(&entries, pattern);
        }
        "syscalls" => {
            cmd_syscalls(&entries);
        }
        "summary" => {
            cmd_summary(&entries);
        }
        _ => {
            eprintln!("Unknown command: {}", command);
            std::process::exit(1);
        }
    }

    Ok(())
}

fn parse_entries(content: &str) -> Vec<Entry> {
    let mut results = Vec::new();

    for chunk in content.split("<title>") {
        if let Some(end) = chunk.find("</title>") {
            let title = &chunk[..end];
            if let Some((name, samples, percent)) = parse_title(title) {
                results.push(Entry { name, samples, percent });
            }
        }
    }

    results.sort_by(|a, b| b.percent.partial_cmp(&a.percent).unwrap_or(std::cmp::Ordering::Equal));
    results
}

fn parse_title(title: &str) -> Option<(String, u64, f64)> {
    // Format: "function_name (123,456,789 samples, 12.34%)"
    let paren_start = title.rfind('(')?;
    let name = title[..paren_start].trim().to_string();
    let meta = &title[paren_start + 1..];

    let samples_end = meta.find(" samples")?;
    let samples_str = &meta[..samples_end].replace(',', "");
    let samples: u64 = samples_str.parse().ok()?;

    let pct_start = meta.rfind(", ")? + 2;
    let pct_end = meta.rfind('%')?;
    let percent: f64 = meta[pct_start..pct_end].parse().ok()?;

    if name.is_empty() || name == "all" {
        return None;
    }

    Some((name, samples, percent))
}

fn cmd_top(entries: &[Entry], n: usize, min_pct: f64) {
    println!("Top {} functions (>= {:.1}%):\n", n, min_pct);
    println!("{:>7}  {}", "%", "Function");
    println!("{}", "-".repeat(80));

    let mut shown = 0;
    let mut total = 0.0;

    for e in entries {
        if e.percent < min_pct {
            continue;
        }
        if shown >= n {
            break;
        }

        let display_name = truncate_name(&e.name, 70);
        println!("{:>6.2}%  {}", e.percent, display_name);
        total += e.percent;
        shown += 1;
    }

    println!("{}", "-".repeat(80));
    println!("{:>6.2}%  Total ({} functions shown)", total, shown);
}

fn cmd_search(entries: &[Entry], pattern: &str) {
    let pattern_lower = pattern.to_lowercase();
    println!("Functions matching '{}':\n", pattern);
    println!("{:>7}  {}", "%", "Function");
    println!("{}", "-".repeat(80));

    let mut total = 0.0;
    let mut count = 0;

    for e in entries {
        if e.name.to_lowercase().contains(&pattern_lower) {
            let display_name = truncate_name(&e.name, 70);
            println!("{:>6.2}%  {}", e.percent, display_name);
            total += e.percent;
            count += 1;
        }
    }

    println!("{}", "-".repeat(80));
    println!("{:>6.2}%  Total ({} matches)", total, count);
}

fn cmd_syscalls(entries: &[Entry]) {
    println!("Syscall breakdown:\n");
    println!("{:>7}  {}", "%", "Syscall");
    println!("{}", "-".repeat(60));

    let mut total = 0.0;

    for e in entries {
        if e.name.starts_with("__x64_sys_") || e.name.starts_with("__x86_sys_") {
            let syscall_name = e.name
                .strip_prefix("__x64_sys_")
                .or_else(|| e.name.strip_prefix("__x86_sys_"))
                .unwrap_or(&e.name);
            println!("{:>6.2}%  {}", e.percent, syscall_name);
            total += e.percent;
        }
    }

    println!("{}", "-".repeat(60));
    println!("{:>6.2}%  Total syscall time", total);
}

fn cmd_summary(entries: &[Entry]) {
    let mut categories: HashMap<&str, f64> = HashMap::new();

    for e in entries {
        let cat = categorize(&e.name);
        *categories.entry(cat).or_insert(0.0) += e.percent;
    }

    let mut cats: Vec<_> = categories.into_iter().collect();
    cats.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("Category summary:\n");
    println!("{:>7}  {}", "%", "Category");
    println!("{}", "-".repeat(40));

    for (cat, pct) in &cats {
        println!("{:>6.2}%  {}", pct, cat);
    }

    println!("\n{}", "=".repeat(60));
    println!("Key functions by category:\n");

    for cat in &[
        "Cache/Foyer",
        "TLS/Crypto",
        "Network I/O",
        "Disk I/O",
        "Tokio Runtime",
        "Locks/Futex",
        "NNTP Protocol",
        "Compression",
    ] {
        let funcs: Vec<_> = entries
            .iter()
            .filter(|e| categorize(&e.name) == *cat && e.percent >= 0.5)
            .take(5)
            .collect();

        if !funcs.is_empty() {
            println!("{}:", cat);
            for e in funcs {
                let short = truncate_name(&e.name, 55);
                println!("  {:>5.2}%  {}", e.percent, short);
            }
            println!();
        }
    }
}

fn categorize(name: &str) -> &'static str {
    let lower = name.to_lowercase();

    // Cache / foyer (disk cache engine)
    if lower.contains("foyer") || lower.contains("hybrid_cache") || lower.contains("hybridarticle")
        || lower.contains("article_cache") || lower.contains("unified_cache")
        || lower.contains("cache::") || lower.contains("moka")
    {
        return "Cache/Foyer";
    }

    // NNTP protocol handling
    if lower.contains("nntp") || lower.contains("precheck") || lower.contains("article_routing")
        || lower.contains("client_session") || lower.contains("backend_execution")
        || lower.contains("command_guard") || lower.contains("route_command")
        || lower.contains("status_code") || lower.contains("message_id")
    {
        return "NNTP Protocol";
    }

    // TLS / crypto
    if lower.contains("tls") || lower.contains("ssl") || lower.contains("rustls")
        || lower.contains("aes") || lower.contains("cipher") || lower.contains("encrypt")
        || lower.contains("decrypt") || lower.contains("handshake") || lower.contains("aws_lc")
        || lower.contains("ring::") || lower.contains("chacha")
    {
        return "TLS/Crypto";
    }

    // Compression (LZ4 for foyer disk cache)
    if lower.contains("lz4") || lower.contains("compress") || lower.contains("decompress")
        || lower.contains("zstd")
    {
        return "Compression";
    }

    // Connection pooling
    if lower.contains("deadpool") || lower.contains("pool") || lower.contains("connection_provider")
    {
        return "Connection Pool";
    }

    // Network I/O
    if lower.contains("recv") || lower.contains("send") || lower.contains("tcp")
        || lower.contains("socket") || lower.contains("inet") || lower.contains("skb")
        || lower.contains("net_")
    {
        return "Network I/O";
    }

    // Disk I/O
    if lower.contains("zfs") || lower.contains("zpl") || lower.contains("zil")
        || lower.contains("vfs") || lower.contains("write_all") || lower.contains("ext4")
        || lower.contains("xfs") || lower.contains("btrfs") || lower.contains("block_")
        || lower.contains("io_uring") || lower.contains("pread") || lower.contains("pwrite")
    {
        return "Disk I/O";
    }

    // Locks / synchronization
    if lower.contains("futex") || lower.contains("mutex") || lower.contains("lock")
        || lower.contains("rwlock") || lower.contains("semaphore") || lower.contains("parking_lot")
    {
        return "Locks/Futex";
    }

    // Event loop
    if lower.contains("epoll") || lower.contains("poll") || lower.contains("mio") {
        return "Event Loop";
    }

    // Tokio runtime
    if lower.contains("tokio") || lower.contains("runtime") {
        return "Tokio Runtime";
    }

    // Async machinery
    if lower.contains("futures") || lower.contains("async") || lower.contains("waker") {
        return "Async/Futures";
    }

    // Scheduling
    if lower.contains("schedule") || lower.contains("switch") || lower.contains("context") {
        return "Scheduling";
    }

    // Memory allocation
    if lower.contains("alloc") || lower.contains("malloc") || lower.contains("free")
        || lower.contains("mmap") || lower.contains("brk") || lower.contains("jemalloc")
    {
        return "Memory";
    }

    // Raw syscalls
    if name.starts_with("__x64_sys_") || name.starts_with("syscall")
        || name.starts_with("do_syscall") || name.starts_with("entry_SYSCALL")
    {
        return "Syscall";
    }

    "Other"
}

fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}...", &name[..max_len - 3])
    }
}
