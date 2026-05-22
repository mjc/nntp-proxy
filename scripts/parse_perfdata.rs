use std::collections::HashMap;
use std::env;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

type SymbolId = usize;
type FastHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[derive(Default)]
struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.hash;
        for byte in bytes {
            hash = hash.rotate_left(5) ^ u64::from(*byte);
            hash = hash.wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        self.hash = hash;
    }
}

fn main() {
    let config = Config::parse();

    if config.help {
        print_usage(&config.program);
        return;
    }

    let report = match config.input {
        Input::Stdin => {
            let stdin = io::stdin();
            parse_perf_script(stdin.lock())
        }
        Input::PerfData(path) => parse_perf_data(&path, config.max_stack),
    };

    let report = match report {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    if report.total_samples == 0 {
        eprintln!("No samples parsed. Pass a perf.data file or pipe `perf script` output.");
        std::process::exit(1);
    }

    print_thread_breakdown(&report);
    println!();
    print_top_functions(&report, 40);
    println!();
    print_top_functions_per_thread(&report, 15);
    println!();
    print_callee_edges(&report, 30);
    println!();
    print_timeline(&report, 10);
    println!();
    print_category_summary(&report);
}

struct Config {
    program: String,
    input: Input,
    max_stack: Option<usize>,
    help: bool,
}

enum Input {
    Stdin,
    PerfData(String),
}

impl Config {
    fn parse() -> Self {
        let mut args = env::args();
        let program = args.next().unwrap_or_else(|| "parse_perfdata".to_string());
        let mut max_stack = None;
        let mut input = None;

        while let Some(arg) = args.next() {
            if arg == "-h" || arg == "--help" {
                return Self {
                    program,
                    input: Input::Stdin,
                    max_stack,
                    help: true,
                };
            }

            if arg == "--max-stack" {
                let Some(value) = args.next() else {
                    eprintln!("--max-stack requires a value");
                    print_usage(&program);
                    std::process::exit(2);
                };
                max_stack = Some(parse_max_stack(&program, &value));
                continue;
            }

            if let Some(value) = arg.strip_prefix("--max-stack=") {
                max_stack = Some(parse_max_stack(&program, value));
                continue;
            }

            if arg.starts_with('-') && arg != "-" {
                eprintln!("unknown option: {arg}");
                print_usage(&program);
                std::process::exit(2);
            }

            if input.replace(arg).is_some() {
                eprintln!("only one perf.data path may be provided");
                print_usage(&program);
                std::process::exit(2);
            }
        }

        let input = match input {
            Some(input) if input == "-" => Input::Stdin,
            Some(input) => Input::PerfData(input),
            None => Input::Stdin,
        };

        Self {
            program,
            input,
            max_stack,
            help: false,
        }
    }
}

fn print_usage(program: &str) {
    eprintln!("Usage:");
    eprintln!("  {program} [--max-stack N] [perf.data]");
    eprintln!("  perf script -i perf.data | {program} -");
    eprintln!();
    eprintln!("With a perf.data path, this runs `perf script -i <path>` and parses the stream.");
    eprintln!("With no argument or `-`, this reads existing `perf script` output from stdin.");
    eprintln!("Full stacks are parsed by default; --max-stack is only an explicit escape hatch.");
}

fn parse_max_stack(program: &str, value: &str) -> usize {
    match value.parse::<usize>() {
        Ok(value) if value > 0 => value,
        _ => {
            eprintln!("invalid --max-stack value: {value}");
            print_usage(program);
            std::process::exit(2);
        }
    }
}

fn parse_perf_data(path: &str, max_stack: Option<usize>) -> Result<Report, String> {
    if !Path::new(path).is_file() {
        return Err(format!("perf.data file not found: {path}"));
    }

    let mut command = Command::new("perf");
    command.arg("script").arg("-i").arg(path);
    if let Some(max_stack) = max_stack {
        command.arg("--max-stack").arg(max_stack.to_string());
    }

    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run `perf script -i {path}`: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture perf script stdout".to_string())?;

    let report = parse_perf_script(BufReader::with_capacity(1024 * 1024, stdout))?;
    let status = child
        .wait()
        .map_err(|err| format!("failed waiting for perf script: {err}"))?;

    if !status.success() {
        return Err(format!("perf script exited with {status}"));
    }

    Ok(report)
}

fn parse_perf_script<R: BufRead>(mut reader: R) -> Result<Report, String> {
    let mut report = Report::default();
    let mut current_comm = None;
    let mut current_tid = 0;
    let mut current_time = 0.0;
    let mut current_stack = Vec::with_capacity(96);
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let len = reader
            .read_until(b'\n', &mut line)
            .map_err(|err| format!("failed reading perf script output: {err}"))?;
        if len == 0 {
            break;
        }

        trim_line_end(&mut line);

        if line.is_empty() {
            if let Some(comm) = current_comm {
                if !current_stack.is_empty() {
                    report.add_sample(comm, current_tid, current_time, &current_stack);
                }
            }
            current_comm = None;
            current_stack.clear();
            continue;
        }

        if line[0] != b'\t' && line[0] != b' ' {
            if let Some((comm, tid, time)) = parse_header_bytes(&line) {
                current_comm = Some(report.intern_bytes(comm));
                current_tid = tid;
                current_time = time;
                current_stack.clear();
            }
            continue;
        }

        if let Some(func) = parse_frame_bytes(trim_ascii(&line)) {
            current_stack.push(report.intern_bytes(func));
        }
    }

    if let Some(comm) = current_comm {
        if !current_stack.is_empty() {
            report.add_sample(comm, current_tid, current_time, &current_stack);
        }
    }

    Ok(report)
}

fn trim_line_end(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ThreadKey {
    tid: u32,
    comm: SymbolId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ThreadLeafKey {
    thread: ThreadKey,
    leaf: SymbolId,
}

#[derive(Default)]
struct Report {
    total_samples: usize,
    symbols: Vec<String>,
    symbol_ids: FastHashMap<String, SymbolId>,
    symbol_categories: Vec<Option<&'static str>>,
    by_thread: FastHashMap<ThreadKey, usize>,
    leaf_counts: FastHashMap<SymbolId, usize>,
    per_thread_leaf: FastHashMap<ThreadLeafKey, usize>,
    edges: FastHashMap<u64, usize>,
    categories: FastHashMap<&'static str, usize>,
    tid_to_comm: FastHashMap<u32, SymbolId>,
    min_time: f64,
    max_time: f64,
    timeline_samples: Vec<TimelineSample>,
}

struct TimelineSample {
    time: f64,
    tid: u32,
    leaf: SymbolId,
}

impl Report {
    fn intern_bytes(&mut self, name: &[u8]) -> SymbolId {
        let Ok(name) = std::str::from_utf8(name) else {
            return self.intern("<invalid utf8>");
        };
        self.intern(name)
    }

    fn intern(&mut self, name: &str) -> SymbolId {
        if let Some(id) = self.symbol_ids.get(name) {
            return *id;
        }

        let id = self.symbols.len();
        self.symbols.push(name.to_string());
        self.symbol_categories.push(None);
        self.symbol_ids.insert(self.symbols[id].clone(), id);
        id
    }

    fn symbol(&self, id: SymbolId) -> &str {
        &self.symbols[id]
    }

    fn category(&mut self, id: SymbolId) -> &'static str {
        if let Some(category) = self.symbol_categories[id] {
            return category;
        }

        let category = categorize(&self.symbols[id]);
        self.symbol_categories[id] = Some(category);
        category
    }

    fn add_sample(&mut self, comm: SymbolId, tid: u32, time: f64, stack: &[SymbolId]) {
        let Some(&leaf) = stack.first() else {
            return;
        };

        if self.total_samples == 0 {
            self.min_time = time;
            self.max_time = time;
        } else {
            self.min_time = self.min_time.min(time);
            self.max_time = self.max_time.max(time);
        }

        self.total_samples += 1;

        let key = ThreadKey { tid, comm };
        *self.by_thread.entry(key).or_insert(0) += 1;
        *self.leaf_counts.entry(leaf).or_insert(0) += 1;
        let category = self.category(leaf);
        *self.categories.entry(category).or_insert(0) += 1;
        self.tid_to_comm.entry(tid).or_insert(comm);

        *self
            .per_thread_leaf
            .entry(ThreadLeafKey { thread: key, leaf })
            .or_insert(0) += 1;

        for w in stack.windows(2) {
            let callee = w[0];
            let caller = w[1];
            *self.edges.entry(pack_edge(caller, callee)).or_insert(0) += 1;
        }

        self.timeline_samples
            .push(TimelineSample { time, tid, leaf });
    }
}

fn pack_edge(caller: SymbolId, callee: SymbolId) -> u64 {
    ((caller as u64) << 32) | callee as u64
}

fn unpack_edge(edge: u64) -> (SymbolId, SymbolId) {
    ((edge >> 32) as SymbolId, (edge & 0xffff_ffff) as SymbolId)
}

fn parse_header_bytes(line: &[u8]) -> Option<(&[u8], u32, f64)> {
    let first_colon = memchr(line, b':')?;
    let before_colon = trim_ascii_end(&line[..first_colon]);

    let ts_space = memrchr(before_colon, b' ')?;
    let time = parse_f64_ascii(&before_colon[ts_space + 1..])?;

    let mut prefix = trim_ascii_end(&before_colon[..ts_space]);
    if prefix.ends_with(b"]") {
        let bracket = memrchr(prefix, b'[')?;
        prefix = trim_ascii_end(&prefix[..bracket]);
    }

    let last_space = memrchr(prefix, b' ')?;
    let comm = trim_ascii(&prefix[..last_space]);
    let tid_bytes = &prefix[last_space + 1..];
    let tid_bytes = if let Some(slash) = memchr(tid_bytes, b'/') {
        &tid_bytes[slash + 1..]
    } else {
        tid_bytes
    };
    let tid = parse_u32_ascii(tid_bytes)?;

    Some((comm, tid, time))
}

fn parse_frame_bytes(line: &[u8]) -> Option<&[u8]> {
    let first_space = memchr(line, b' ')?;
    let mut rest = trim_ascii(&line[first_space + 1..]);

    if let Some(paren) = find_last_space_paren(rest) {
        rest = &rest[..paren];
    }

    let func = if let Some(plus) = memrchr(rest, b'+') {
        let after = &rest[plus + 1..];
        if after.starts_with(b"0x") || after.iter().all(u8::is_ascii_hexdigit) {
            &rest[..plus]
        } else {
            rest
        }
    } else {
        rest
    };

    if func.is_empty() {
        None
    } else {
        Some(func)
    }
}

fn find_last_space_paren(bytes: &[u8]) -> Option<usize> {
    let mut idx = bytes.len();
    while idx >= 2 {
        idx -= 1;
        if bytes[idx] == b'(' && bytes[idx - 1] == b' ' {
            return Some(idx - 1);
        }
    }
    None
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    trim_ascii_end(trim_ascii_start(bytes))
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if !first.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

fn trim_ascii_end(mut bytes: &[u8]) -> &[u8] {
    while let Some((&last, rest)) = bytes.split_last() {
        if !last.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

fn memchr(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|byte| *byte == needle)
}

fn memrchr(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().rposition(|byte| *byte == needle)
}

fn parse_u32_ascii(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    let mut saw_digit = false;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        saw_digit = true;
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    saw_digit.then_some(value)
}

fn parse_f64_ascii(bytes: &[u8]) -> Option<f64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn print_thread_breakdown(report: &Report) {
    let mut threads: Vec<_> = report.by_thread.iter().collect();
    threads.sort_by(|a, b| b.1.cmp(a.1));

    println!(
        "═══ Thread Breakdown ({} total samples) ═══\n",
        report.total_samples
    );
    println!("{:>7} {:>10}  {:>7}  {}", "%", "samples", "tid", "comm");
    println!("{}", "-".repeat(60));

    for (thread, count) in threads {
        let pct = *count as f64 / report.total_samples as f64 * 100.0;
        println!(
            "{:>6.2}% {:>10}  {:>7}  {}",
            pct,
            count,
            thread.tid,
            report.symbol(thread.comm)
        );
    }
}

fn print_top_functions(report: &Report, n: usize) {
    let mut funcs: Vec<_> = report.leaf_counts.iter().collect();
    funcs.sort_by(|a, b| b.1.cmp(a.1));

    println!("═══ Top {} Functions (self/on-CPU time) ═══\n", n);
    println!("{:>7} {:>10}  {}", "%", "samples", "Function");
    println!("{}", "-".repeat(100));

    let mut shown_pct = 0.0;
    for (func, count) in funcs.iter().take(n) {
        let pct = **count as f64 / report.total_samples as f64 * 100.0;
        println!(
            "{:>6.2}% {:>10}  {}",
            pct,
            count,
            truncate(report.symbol(**func), 80)
        );
        shown_pct += pct;
    }
    println!("{}", "-".repeat(100));
    println!(
        "{:>6.2}%             Total ({} functions shown)",
        shown_pct,
        funcs.len().min(n)
    );
}

fn print_top_functions_per_thread(report: &Report, n: usize) {
    let mut threads: Vec<_> = report.by_thread.iter().collect();
    threads.sort_by(|a, b| b.1.cmp(a.1));

    println!("═══ Top Functions Per Thread ═══");

    for (thread, thread_total) in threads.into_iter().take(8) {
        let mut funcs: Vec<_> = report
            .per_thread_leaf
            .iter()
            .filter_map(|(key, count)| (key.thread == *thread).then_some((&key.leaf, count)))
            .collect();
        funcs.sort_by(|a, b| b.1.cmp(a.1));

        println!(
            "\n─── {} (tid {}, {} samples) ───\n",
            report.symbol(thread.comm),
            thread.tid,
            thread_total
        );
        println!("{:>7} {:>10}  {}", "%", "samples", "Function");

        for (func, count) in funcs.iter().take(n) {
            let pct = **count as f64 / *thread_total as f64 * 100.0;
            println!(
                "{:>6.2}% {:>10}  {}",
                pct,
                count,
                truncate(report.symbol(**func), 75)
            );
        }
    }
}

fn print_callee_edges(report: &Report, n: usize) {
    let mut edge_list: Vec<_> = report.edges.iter().collect();
    edge_list.sort_by(|a, b| b.1.cmp(a.1));

    println!("═══ Top {} Caller → Callee Edges ═══\n", n);
    println!("{:>7} {:>10}  {} → {}", "%", "samples", "Caller", "Callee");
    println!("{}", "-".repeat(120));

    for (edge, count) in edge_list.iter().take(n) {
        let (caller, callee) = unpack_edge(**edge);
        let pct = **count as f64 / report.total_samples as f64 * 100.0;
        println!(
            "{:>6.2}% {:>10}  {} → {}",
            pct,
            count,
            truncate(report.symbol(caller), 50),
            truncate(report.symbol(callee), 50)
        );
    }
}

fn print_timeline(report: &Report, buckets: usize) {
    let duration = report.max_time - report.min_time;

    if duration <= 0.0 {
        println!("═══ Timeline ═══\n");
        println!("All samples at same timestamp — cannot bucket.");
        return;
    }

    let bucket_width = duration / buckets as f64;

    struct Bucket {
        start: f64,
        total: usize,
        by_thread: FastHashMap<u32, usize>,
        top_funcs: FastHashMap<SymbolId, usize>,
    }

    let mut bucket_vec: Vec<Bucket> = (0..buckets)
        .map(|i| Bucket {
            start: report.min_time + i as f64 * bucket_width,
            total: 0,
            by_thread: FastHashMap::default(),
            top_funcs: FastHashMap::default(),
        })
        .collect();

    for sample in &report.timeline_samples {
        let idx = ((sample.time - report.min_time) / bucket_width) as usize;
        let idx = idx.min(buckets - 1);
        let bucket = &mut bucket_vec[idx];
        bucket.total += 1;
        *bucket.by_thread.entry(sample.tid).or_insert(0) += 1;
        *bucket.top_funcs.entry(sample.leaf).or_insert(0) += 1;
    }

    let mut top_threads: Vec<_> = report
        .by_thread
        .iter()
        .map(|(thread, count)| (thread.tid, *count))
        .collect();
    top_threads.sort_by(|a, b| b.1.cmp(&a.1));
    let top_threads: Vec<u32> = top_threads.iter().take(6).map(|t| t.0).collect();

    println!(
        "═══ Timeline ({:.1}s duration, {} buckets) ═══\n",
        duration, buckets
    );
    println!(
        "This shows sample distribution over time to distinguish cold (early) vs hot (late) phases.\n"
    );

    print!("{:>12} {:>8}", "Time(s)", "Samples");
    for tid in &top_threads {
        let name = report
            .tid_to_comm
            .get(tid)
            .map(|comm| report.symbol(*comm))
            .unwrap_or("?");
        let label = if name.len() > 10 { &name[..10] } else { name };
        print!("  {:>10}", label);
    }
    println!("  Top function");
    println!("{}", "-".repeat(120));

    for bucket in &bucket_vec {
        let offset = bucket.start - report.min_time;
        print!(
            "{:>8.1}-{:<3.1} {:>8}",
            offset,
            offset + bucket_width,
            bucket.total
        );

        for tid in &top_threads {
            let count = bucket.by_thread.get(tid).copied().unwrap_or(0);
            let pct = if bucket.total > 0 {
                count as f64 / bucket.total as f64 * 100.0
            } else {
                0.0
            };
            print!("  {:>7.1}%  ", pct);
        }

        if let Some((func, _count)) = bucket.top_funcs.iter().max_by_key(|(_k, v)| *v) {
            print!("  {}", truncate(report.symbol(*func), 40));
        }

        println!();
    }

    let mid = buckets / 2;
    let first_half: usize = bucket_vec[..mid].iter().map(|b| b.total).sum();
    let second_half: usize = bucket_vec[mid..].iter().map(|b| b.total).sum();

    println!();
    println!(
        "First half: {} samples ({:.1}%), Second half: {} samples ({:.1}%)",
        first_half,
        first_half as f64 / report.total_samples as f64 * 100.0,
        second_half,
        second_half as f64 / report.total_samples as f64 * 100.0,
    );

    let mut first_funcs: FastHashMap<SymbolId, usize> = FastHashMap::default();
    let mut second_funcs: FastHashMap<SymbolId, usize> = FastHashMap::default();

    for sample in &report.timeline_samples {
        let idx = ((sample.time - report.min_time) / bucket_width) as usize;
        let idx = idx.min(buckets - 1);
        if idx < mid {
            *first_funcs.entry(sample.leaf).or_insert(0) += 1;
        } else {
            *second_funcs.entry(sample.leaf).or_insert(0) += 1;
        }
    }

    println!("\nFunctions hotter in FIRST half (cold phase):");
    print_phase_diff(
        report,
        &first_funcs,
        &second_funcs,
        first_half,
        second_half,
        10,
    );

    println!("\nFunctions hotter in SECOND half (hot/cached phase):");
    print_phase_diff(
        report,
        &second_funcs,
        &first_funcs,
        second_half,
        first_half,
        10,
    );
}

fn print_phase_diff(
    report: &Report,
    primary: &FastHashMap<SymbolId, usize>,
    other: &FastHashMap<SymbolId, usize>,
    primary_total: usize,
    other_total: usize,
    n: usize,
) {
    if primary_total == 0 || other_total == 0 {
        println!("  (insufficient data)");
        return;
    }

    let mut diffs: Vec<(SymbolId, f64, f64, f64)> = Vec::new();
    for (&func, &count) in primary {
        let pct_primary = count as f64 / primary_total as f64 * 100.0;
        let pct_other = other.get(&func).copied().unwrap_or(0) as f64 / other_total as f64 * 100.0;
        let diff = pct_primary - pct_other;
        if diff > 0.1 {
            diffs.push((func, pct_primary, pct_other, diff));
        }
    }
    diffs.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "{:>7} {:>7} {:>7}  {}",
        "this%", "other%", "diff%", "Function"
    );
    for (func, pct_p, pct_o, diff) in diffs.iter().take(n) {
        println!(
            "{:>6.2}% {:>6.2}% {:>+6.2}%  {}",
            pct_p,
            pct_o,
            diff,
            truncate(report.symbol(*func), 70)
        );
    }
}

fn print_category_summary(report: &Report) {
    let mut cats: Vec<_> = report.categories.iter().collect();
    cats.sort_by(|a, b| b.1.cmp(a.1));

    println!("═══ Category Summary (self time) ═══\n");
    println!("{:>7} {:>10}  {}", "%", "samples", "Category");
    println!("{}", "-".repeat(40));

    for (cat, count) in cats {
        let pct = *count as f64 / report.total_samples as f64 * 100.0;
        println!("{:>6.2}% {:>10}  {}", pct, count, cat);
    }
}

fn categorize(name: &str) -> &'static str {
    let lower = name.to_lowercase();

    if lower.contains("foyer")
        || lower.contains("hybrid_cache")
        || lower.contains("hybridarticle")
        || lower.contains("article_cache")
        || lower.contains("unified_cache")
        || lower.contains("cache::")
        || lower.contains("moka")
    {
        return "Cache/Foyer";
    }

    if lower.contains("nntp")
        || lower.contains("precheck")
        || lower.contains("article_routing")
        || lower.contains("client_session")
        || lower.contains("backend_execution")
        || lower.contains("command_guard")
        || lower.contains("route_command")
        || lower.contains("status_code")
        || lower.contains("message_id")
    {
        return "NNTP Protocol";
    }

    if lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("rustls")
        || lower.contains("aes")
        || lower.contains("cipher")
        || lower.contains("encrypt")
        || lower.contains("decrypt")
        || lower.contains("handshake")
        || lower.contains("aws_lc")
        || lower.contains("ring::")
        || lower.contains("chacha")
    {
        return "TLS/Crypto";
    }

    if lower.contains("lz4")
        || lower.contains("compress")
        || lower.contains("decompress")
        || lower.contains("zstd")
    {
        return "Compression";
    }

    if lower.contains("deadpool") || lower.contains("pool") || lower.contains("connection_provider")
    {
        return "Connection Pool";
    }

    if lower.contains("recv")
        || lower.contains("send")
        || lower.contains("tcp")
        || lower.contains("socket")
        || lower.contains("inet")
        || lower.contains("skb")
        || lower.contains("net_")
    {
        return "Network I/O";
    }

    if lower.contains("zfs")
        || lower.contains("zpl")
        || lower.contains("zil")
        || lower.contains("vfs")
        || lower.contains("write_all")
        || lower.contains("ext4")
        || lower.contains("xfs")
        || lower.contains("btrfs")
        || lower.contains("block_")
        || lower.contains("io_uring")
        || lower.contains("pread")
        || lower.contains("pwrite")
    {
        return "Disk I/O";
    }

    if lower.contains("futex")
        || lower.contains("mutex")
        || lower.contains("lock")
        || lower.contains("rwlock")
        || lower.contains("semaphore")
        || lower.contains("parking_lot")
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

    if lower.contains("alloc")
        || lower.contains("malloc")
        || lower.contains("free")
        || lower.contains("mmap")
        || lower.contains("brk")
        || lower.contains("jemalloc")
    {
        return "Memory";
    }

    if name.starts_with("__x64_sys_")
        || name.starts_with("syscall")
        || name.starts_with("do_syscall")
        || name.starts_with("entry_SYSCALL")
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
