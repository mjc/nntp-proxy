use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Read, Seek, SeekFrom};

type SymbolId = usize;
type FastHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

const PERF_MAGIC: &[u8; 8] = b"PERFILE2";
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_SAMPLE: u32 = 9;

const PERF_SAMPLE_IP: u64 = 1 << 0;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_ADDR: u64 = 1 << 3;
const PERF_SAMPLE_READ: u64 = 1 << 4;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const PERF_SAMPLE_ID: u64 = 1 << 6;
const PERF_SAMPLE_CPU: u64 = 1 << 7;
const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
const PERF_SAMPLE_STREAM_ID: u64 = 1 << 9;
const PERF_SAMPLE_RAW: u64 = 1 << 10;
const PERF_SAMPLE_BRANCH_STACK: u64 = 1 << 11;
const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
const PERF_SAMPLE_STACK_USER: u64 = 1 << 13;
const PERF_SAMPLE_WEIGHT: u64 = 1 << 14;
const PERF_SAMPLE_DATA_SRC: u64 = 1 << 15;
const PERF_SAMPLE_IDENTIFIER: u64 = 1 << 16;
const PERF_SAMPLE_TRANSACTION: u64 = 1 << 17;
const PERF_SAMPLE_REGS_INTR: u64 = 1 << 18;
const PERF_SAMPLE_PHYS_ADDR: u64 = 1 << 19;
const PERF_SAMPLE_AUX: u64 = 1 << 20;
const PERF_SAMPLE_CGROUP: u64 = 1 << 21;
const PERF_SAMPLE_DATA_PAGE_SIZE: u64 = 1 << 22;
const PERF_SAMPLE_CODE_PAGE_SIZE: u64 = 1 << 23;
const PERF_SAMPLE_WEIGHT_STRUCT: u64 = 1 << 24;

const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
const PERF_FORMAT_ID: u64 = 1 << 2;
const PERF_FORMAT_GROUP: u64 = 1 << 3;
const PERF_FORMAT_LOST: u64 = 1 << 4;

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

    let report = match parse_perf_data(&config.input, config.max_stack) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    if report.total_samples == 0 {
        eprintln!("No samples parsed from perf.data.");
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
    input: String,
    max_stack: Option<usize>,
    help: bool,
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
                    input: "perf.data".to_string(),
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

            if arg.starts_with('-') {
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

        Self {
            program,
            input: input.unwrap_or_else(|| "perf.data".to_string()),
            max_stack,
            help: false,
        }
    }
}

fn print_usage(program: &str) {
    eprintln!("Usage:");
    eprintln!("  {program} [--max-stack N] [perf.data]");
    eprintln!();
    eprintln!("Reads Linux perf.data directly, without invoking perf's text renderer.");
    eprintln!("Full callchains are parsed by default; --max-stack caps unusually large stacks.");
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

#[derive(Clone, Copy)]
struct PerfHeader {
    attr_offset: u64,
    attr_size: u64,
    data_offset: u64,
    data_size: u64,
}

#[derive(Clone, Copy)]
struct PerfAttr {
    sample_type: u64,
    read_format: u64,
    sample_regs_user: u64,
    sample_regs_intr: u64,
}

fn parse_perf_data(path: &str, max_stack: Option<usize>) -> Result<Report, String> {
    let mut file = File::open(path).map_err(|err| format!("failed opening {path}: {err}"))?;
    let header = read_header(&mut file)?;
    let attr = read_first_attr(&mut file, header)?;
    let mut report = Report::default();

    file.seek(SeekFrom::Start(header.data_offset))
        .map_err(|err| format!("failed seeking to perf data section: {err}"))?;

    let mut remaining = header.data_size;
    let mut record_header = [0u8; 8];
    let mut payload = Vec::with_capacity(4096);
    let mut stack = Vec::with_capacity(max_stack.unwrap_or(96).min(4096));

    while remaining >= 8 {
        file.read_exact(&mut record_header)
            .map_err(|err| format!("failed reading record header: {err}"))?;
        remaining -= 8;

        let record_type = read_u32_at(&record_header, 0);
        let record_size = u64::from(read_u16_at(&record_header, 6));
        if record_size < 8 {
            return Err(format!("invalid perf record size: {record_size}"));
        }

        let payload_size = record_size - 8;
        if payload_size > remaining {
            return Err(format!(
                "perf record overruns data section: size={record_size} remaining={remaining}"
            ));
        }

        payload.resize(payload_size as usize, 0);
        file.read_exact(&mut payload)
            .map_err(|err| format!("failed reading record payload: {err}"))?;
        remaining -= payload_size;

        match record_type {
            PERF_RECORD_COMM => parse_comm_record(&payload, &mut report),
            PERF_RECORD_SAMPLE => {
                stack.clear();
                if let Some(sample) = parse_sample_record(&payload, attr, max_stack, &mut stack) {
                    let comm = report.comm_for_tid(sample.tid);
                    report.add_sample(comm, sample.tid, sample.time, sample.stack);
                }
            }
            _ => {}
        }
    }

    Ok(report)
}

fn read_header(file: &mut File) -> Result<PerfHeader, String> {
    let mut buf = [0u8; 104];
    file.read_exact(&mut buf)
        .map_err(|err| format!("failed reading perf header: {err}"))?;

    if &buf[..8] != PERF_MAGIC {
        return Err("not a PERFILE2 perf.data file".to_string());
    }

    let header_size = read_u64_at(&buf, 8);
    if header_size < 104 {
        return Err(format!("unsupported perf header size: {header_size}"));
    }

    Ok(PerfHeader {
        attr_offset: read_u64_at(&buf, 24),
        attr_size: read_u64_at(&buf, 32),
        data_offset: read_u64_at(&buf, 40),
        data_size: read_u64_at(&buf, 48),
    })
}

fn read_first_attr(file: &mut File, header: PerfHeader) -> Result<PerfAttr, String> {
    if header.attr_size < 112 {
        return Err(format!("unsupported perf attr size: {}", header.attr_size));
    }

    let mut buf = vec![0u8; header.attr_size as usize];
    file.seek(SeekFrom::Start(header.attr_offset))
        .map_err(|err| format!("failed seeking to perf attrs: {err}"))?;
    file.read_exact(&mut buf)
        .map_err(|err| format!("failed reading perf attrs: {err}"))?;

    Ok(PerfAttr {
        sample_type: read_u64_at(&buf, 24),
        read_format: read_u64_at(&buf, 32),
        sample_regs_user: read_u64_at(&buf, 80),
        sample_regs_intr: read_u64_at(&buf, 104),
    })
}

struct Sample<'a> {
    tid: u32,
    time: f64,
    stack: &'a [u64],
}

fn parse_sample_record<'a>(
    payload: &[u8],
    attr: PerfAttr,
    max_stack: Option<usize>,
    stack: &'a mut Vec<u64>,
) -> Option<Sample<'a>> {
    let mut cursor = Cursor::new(payload);
    let mut tid = 0;
    let mut time = 0.0;
    let mut ip = None;

    if attr.sample_type & PERF_SAMPLE_IDENTIFIER != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_IP != 0 {
        ip = Some(cursor.u64()?);
    }
    if attr.sample_type & PERF_SAMPLE_TID != 0 {
        cursor.skip(4)?;
        tid = cursor.u32()?;
    }
    if attr.sample_type & PERF_SAMPLE_TIME != 0 {
        time = cursor.u64()? as f64 / 1_000_000_000.0;
    }
    if attr.sample_type & PERF_SAMPLE_ADDR != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_ID != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_STREAM_ID != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_CPU != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_PERIOD != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_READ != 0 {
        skip_read_format(&mut cursor, attr.read_format)?;
    }
    if attr.sample_type & PERF_SAMPLE_CALLCHAIN != 0 {
        let nr = cursor.u64()? as usize;
        let limit = max_stack.unwrap_or(nr).min(nr);
        for index in 0..nr {
            let frame = cursor.u64()?;
            if index < limit && is_real_ip(frame) {
                stack.push(frame);
            }
        }
    } else if let Some(ip) = ip {
        stack.push(ip);
    }

    if attr.sample_type & PERF_SAMPLE_RAW != 0 {
        let len = cursor.u32()? as usize;
        cursor.skip(align_8(len))?;
    }
    if attr.sample_type & PERF_SAMPLE_BRANCH_STACK != 0 {
        let nr = cursor.u64()? as usize;
        cursor.skip(nr.checked_mul(24)?)?;
    }
    if attr.sample_type & PERF_SAMPLE_REGS_USER != 0 {
        skip_regs(&mut cursor, attr.sample_regs_user)?;
    }
    if attr.sample_type & PERF_SAMPLE_STACK_USER != 0 {
        let size = cursor.u64()? as usize;
        cursor.skip(size)?;
        if size > 0 {
            cursor.skip(8)?;
        }
    }
    if attr.sample_type & PERF_SAMPLE_WEIGHT != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_DATA_SRC != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_TRANSACTION != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_REGS_INTR != 0 {
        skip_regs(&mut cursor, attr.sample_regs_intr)?;
    }
    if attr.sample_type & PERF_SAMPLE_PHYS_ADDR != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_AUX != 0 {
        let size = cursor.u64()? as usize;
        cursor.skip(size)?;
    }
    if attr.sample_type & PERF_SAMPLE_CGROUP != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_DATA_PAGE_SIZE != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_CODE_PAGE_SIZE != 0 {
        cursor.skip(8)?;
    }
    if attr.sample_type & PERF_SAMPLE_WEIGHT_STRUCT != 0 {
        cursor.skip(8)?;
    }

    if stack.is_empty() {
        return None;
    }

    Some(Sample { tid, time, stack })
}

fn is_real_ip(ip: u64) -> bool {
    ip != 0 && ip < 0xffff_ffff_ffff_f000
}

fn skip_read_format(cursor: &mut Cursor<'_>, read_format: u64) -> Option<()> {
    if read_format & PERF_FORMAT_GROUP != 0 {
        let nr = cursor.u64()? as usize;
        if read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
            cursor.skip(8)?;
        }
        if read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
            cursor.skip(8)?;
        }
        let mut fields_per_value = 1usize;
        if read_format & PERF_FORMAT_ID != 0 {
            fields_per_value += 1;
        }
        if read_format & PERF_FORMAT_LOST != 0 {
            fields_per_value += 1;
        }
        cursor.skip(nr.checked_mul(fields_per_value)?.checked_mul(8)?)?;
    } else {
        cursor.skip(8)?;
        if read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
            cursor.skip(8)?;
        }
        if read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
            cursor.skip(8)?;
        }
        if read_format & PERF_FORMAT_ID != 0 {
            cursor.skip(8)?;
        }
        if read_format & PERF_FORMAT_LOST != 0 {
            cursor.skip(8)?;
        }
    }

    Some(())
}

fn skip_regs(cursor: &mut Cursor<'_>, mask: u64) -> Option<()> {
    let abi = cursor.u64()?;
    if abi != 0 {
        cursor.skip(mask.count_ones() as usize * 8)?;
    }
    Some(())
}

fn parse_comm_record(payload: &[u8], report: &mut Report) {
    if payload.len() < 8 {
        return;
    }

    let tid = read_u32_at(payload, 4);
    let name_bytes = &payload[8..];
    let name_end = name_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name_bytes.len());
    if name_end == 0 {
        return;
    }

    let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
    let comm = report.intern(&name);
    report.tid_to_comm.insert(tid, comm);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        self.offset = self.offset.checked_add(len)?;
        (self.offset <= self.bytes.len()).then_some(())
    }

    fn u32(&mut self) -> Option<u32> {
        let value = read_u32_at(self.bytes, self.offset);
        self.skip(4)?;
        Some(value)
    }

    fn u64(&mut self) -> Option<u64> {
        let value = read_u64_at(self.bytes, self.offset);
        self.skip(8)?;
        Some(value)
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

    fn intern_ip(&mut self, ip: u64) -> SymbolId {
        let name = format!("0x{ip:016x}");
        self.intern(&name)
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

    fn comm_for_tid(&mut self, tid: u32) -> SymbolId {
        if let Some(comm) = self.tid_to_comm.get(&tid) {
            return *comm;
        }

        let name = format!("tid {tid}");
        let comm = self.intern(&name);
        self.tid_to_comm.insert(tid, comm);
        comm
    }

    fn add_sample(&mut self, comm: SymbolId, tid: u32, time: f64, stack_ips: &[u64]) {
        let Some(&leaf_ip) = stack_ips.first() else {
            return;
        };
        let leaf = self.intern_ip(leaf_ip);

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

        let mut previous = leaf;
        for ip in &stack_ips[1..] {
            let caller = self.intern_ip(*ip);
            *self.edges.entry(pack_edge(caller, previous)).or_insert(0) += 1;
            previous = caller;
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

fn print_thread_breakdown(report: &Report) {
    let mut threads: Vec<_> = report.by_thread.iter().collect();
    threads.sort_by(|a, b| b.1.cmp(a.1));

    println!(
        "=== Thread Breakdown ({} total samples) ===\n",
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

    println!("=== Top {} Functions (self/on-CPU time) ===\n", n);
    println!("{:>7} {:>10}  {}", "%", "samples", "Instruction pointer");
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

    println!("=== Top Functions Per Thread ===");

    for (thread, thread_total) in threads.into_iter().take(8) {
        let mut funcs: Vec<_> = report
            .per_thread_leaf
            .iter()
            .filter_map(|(key, count)| (key.thread == *thread).then_some((&key.leaf, count)))
            .collect();
        funcs.sort_by(|a, b| b.1.cmp(a.1));

        println!(
            "\n--- {} (tid {}, {} samples) ---\n",
            report.symbol(thread.comm),
            thread.tid,
            thread_total
        );
        println!("{:>7} {:>10}  {}", "%", "samples", "Instruction pointer");

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

    println!("=== Top {} Caller -> Callee Edges ===\n", n);
    println!("{:>7} {:>10}  {} -> {}", "%", "samples", "Caller", "Callee");
    println!("{}", "-".repeat(120));

    for (edge, count) in edge_list.iter().take(n) {
        let (caller, callee) = unpack_edge(**edge);
        let pct = **count as f64 / report.total_samples as f64 * 100.0;
        println!(
            "{:>6.2}% {:>10}  {} -> {}",
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
        println!("=== Timeline ===\n");
        println!("All samples at same timestamp; cannot bucket.");
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
        let idx = ((sample.time - report.min_time) / bucket_width)
            .floor()
            .min((buckets - 1) as f64) as usize;
        let bucket = &mut bucket_vec[idx];
        bucket.total += 1;
        *bucket.by_thread.entry(sample.tid).or_insert(0) += 1;
        *bucket.top_funcs.entry(sample.leaf).or_insert(0) += 1;
    }

    println!(
        "=== Timeline ({} buckets over {:.2}s) ===\n",
        buckets, duration
    );
    println!(
        "{:>10}  {:>8}  {:>20}  {}",
        "time", "samples", "top tid", "top instruction pointer"
    );
    println!("{}", "-".repeat(90));

    for bucket in bucket_vec {
        let top_tid = bucket
            .by_thread
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(tid, count)| format!("{tid} ({count})"))
            .unwrap_or_else(|| "-".to_string());

        let top_func = bucket
            .top_funcs
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(func, count)| format!("{} ({})", truncate(report.symbol(*func), 40), count))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:>9.2}s  {:>8}  {:>20}  {}",
            bucket.start - report.min_time,
            bucket.total,
            top_tid,
            top_func
        );
    }
}

fn print_category_summary(report: &Report) {
    let mut categories: Vec<_> = report.categories.iter().collect();
    categories.sort_by(|a, b| b.1.cmp(a.1));

    println!("=== Category Summary (leaf/self samples) ===\n");
    println!("{:>7} {:>10}  Category", "%", "samples");
    println!("{}", "-".repeat(60));

    for (category, count) in categories {
        let pct = *count as f64 / report.total_samples as f64 * 100.0;
        println!("{:>6.2}% {:>10}  {}", pct, count, category);
    }
}

fn categorize(func: &str) -> &'static str {
    let lower = func.to_ascii_lowercase();
    if lower.contains("syscall")
        || lower.contains("recv")
        || lower.contains("send")
        || lower.contains("epoll")
        || lower.contains("read")
        || lower.contains("write")
        || lower.contains("tcp")
        || lower.contains("socket")
        || lower.contains("0xffff")
    {
        "syscall/kernel/io"
    } else if lower.contains("rustls")
        || lower.contains("ring")
        || lower.contains("crypto")
        || lower.contains("encrypt")
        || lower.contains("decrypt")
        || lower.contains("tls")
    {
        "tls/crypto"
    } else if lower.contains("tokio")
        || lower.contains("poll")
        || lower.contains("wake")
        || lower.contains("task")
        || lower.contains("runtime")
    {
        "tokio/runtime"
    } else if lower.contains("bytes")
        || lower.contains("alloc")
        || lower.contains("malloc")
        || lower.contains("free")
        || lower.contains("memcpy")
        || lower.contains("copy")
    {
        "allocation/copy"
    } else if lower.contains("nntp_proxy")
        || lower.contains("multiline")
        || lower.contains("retry")
        || lower.contains("article")
        || lower.contains("backend")
        || lower.contains("pool")
    {
        "proxy logic"
    } else {
        "other"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 3 {
        ".".repeat(max)
    } else {
        let prefix: String = s.chars().take(max - 3).collect();
        format!("{prefix}...")
    }
}

fn align_8(len: usize) -> usize {
    (len + 7) & !7
}

fn read_u16_at(bytes: &[u8], offset: usize) -> u16 {
    let mut value = [0u8; 2];
    value.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(value)
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0u8; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}
