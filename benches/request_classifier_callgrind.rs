//! Callgrind comparison for NNTP request verb classification.
//!
//! The `main_original_*` benchmarks model the old classifier from `main`:
//! copy the command keyword into a small stack buffer, uppercase it, then test
//! it against command case tables. The `length_bucket_*` benchmarks model the
//! current request-context verb classifier: dispatch by keyword length, then
//! compare bytes in place with `eq_ignore_ascii_case`.
//!
//! Run with: `cargo bench --bench request_classifier_callgrind`

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use iai_callgrind::{
    Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use std::hint::black_box;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchKind {
    Article,
    Body,
    Head,
    Stat,
    Group,
    ListGroup,
    Last,
    Next,
    List,
    Date,
    Help,
    Capabilities,
    Mode,
    Quit,
    Over,
    Xover,
    Hdr,
    Xhdr,
    NewGroups,
    NewNews,
    Post,
    Ihave,
    AuthInfo,
    StartTls,
    Unknown,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const REALISTIC_VERBS: &[&[u8]] = &[
    b"ARTICLE",
    b"BODY",
    b"HEAD",
    b"STAT",
    b"ARTICLE",
    b"BODY",
    b"HEAD",
    b"GROUP",
    b"OVER",
    b"LIST",
    b"DATE",
    b"CAPABILITIES",
    b"QUIT",
];

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const MIXED_CASE_VERBS: &[&[u8]] = &[
    b"Article",
    b"body",
    b"HeAd",
    b"stat",
    b"gRoUp",
    b"listgroup",
    b"Capabilities",
    b"authinfo",
    b"StartTls",
];

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const ALL_VERBS: &[&[u8]] = &[
    b"ARTICLE",
    b"BODY",
    b"HEAD",
    b"STAT",
    b"GROUP",
    b"LISTGROUP",
    b"LAST",
    b"NEXT",
    b"LIST",
    b"DATE",
    b"HELP",
    b"CAPABILITIES",
    b"MODE",
    b"QUIT",
    b"OVER",
    b"XOVER",
    b"HDR",
    b"XHDR",
    b"NEWGROUPS",
    b"NEWNEWS",
    b"POST",
    b"IHAVE",
    b"AUTHINFO",
    b"STARTTLS",
    b"XUNKNOWN",
];

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[inline(never)]
fn classify_length_bucket(verb: &[u8]) -> BenchKind {
    macro_rules! classify_verbs {
        ($verb:expr; $($len:literal => { $($lit:literal => $kind:expr),+ $(,)? }),+ $(,)?) => {{
            match $verb.len() {
                $(
                    $len => {
                        $(
                            const _: [(); $len] = [(); $lit.len()];
                        )+
                        $(
                            if $verb.eq_ignore_ascii_case($lit) {
                                $kind
                            } else
                        )+
                        {
                            BenchKind::Unknown
                        }
                    }
                )+
                _ => BenchKind::Unknown,
            }
        }};
    }

    classify_verbs!(verb;
        3 => {
            b"HDR" => BenchKind::Hdr,
        },
        4 => {
            b"BODY" => BenchKind::Body,
            b"DATE" => BenchKind::Date,
            b"HEAD" => BenchKind::Head,
            b"HELP" => BenchKind::Help,
            b"LAST" => BenchKind::Last,
            b"LIST" => BenchKind::List,
            b"MODE" => BenchKind::Mode,
            b"NEXT" => BenchKind::Next,
            b"OVER" => BenchKind::Over,
            b"POST" => BenchKind::Post,
            b"QUIT" => BenchKind::Quit,
            b"STAT" => BenchKind::Stat,
            b"XHDR" => BenchKind::Xhdr,
        },
        5 => {
            b"GROUP" => BenchKind::Group,
            b"IHAVE" => BenchKind::Ihave,
            b"XOVER" => BenchKind::Xover,
        },
        7 => {
            b"ARTICLE" => BenchKind::Article,
            b"NEWNEWS" => BenchKind::NewNews,
        },
        8 => {
            b"AUTHINFO" => BenchKind::AuthInfo,
            b"STARTTLS" => BenchKind::StartTls,
        },
        9 => {
            b"LISTGROUP" => BenchKind::ListGroup,
            b"NEWGROUPS" => BenchKind::NewGroups,
        },
        12 => {
            b"CAPABILITIES" => BenchKind::Capabilities,
        },
    )
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[inline(never)]
fn matches_any(cmd: &[u8], cases: &[&[u8]; 3]) -> bool {
    cases.contains(&cmd)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[inline(never)]
fn classify_main_original(verb: &[u8]) -> BenchKind {
    let mut cmd_upper_buf = [0u8; 16];
    let cmd = if verb.len() <= 16 {
        cmd_upper_buf[..verb.len()].copy_from_slice(verb);
        cmd_upper_buf[..verb.len()].make_ascii_uppercase();
        &cmd_upper_buf[..verb.len()]
    } else {
        verb
    };

    const ARTICLE: &[&[u8]; 3] = &[b"ARTICLE", b"article", b"Article"];
    const BODY: &[&[u8]; 3] = &[b"BODY", b"body", b"Body"];
    const HEAD: &[&[u8]; 3] = &[b"HEAD", b"head", b"Head"];
    const STAT: &[&[u8]; 3] = &[b"STAT", b"stat", b"Stat"];
    const GROUP: &[&[u8]; 3] = &[b"GROUP", b"group", b"Group"];
    const AUTHINFO: &[&[u8]; 3] = &[b"AUTHINFO", b"authinfo", b"Authinfo"];
    const LIST: &[&[u8]; 3] = &[b"LIST", b"list", b"List"];
    const DATE: &[&[u8]; 3] = &[b"DATE", b"date", b"Date"];
    const CAPABILITIES: &[&[u8]; 3] = &[b"CAPABILITIES", b"capabilities", b"Capabilities"];
    const MODE: &[&[u8]; 3] = &[b"MODE", b"mode", b"Mode"];
    const HELP: &[&[u8]; 3] = &[b"HELP", b"help", b"Help"];
    const QUIT: &[&[u8]; 3] = &[b"QUIT", b"quit", b"Quit"];
    const XOVER: &[&[u8]; 3] = &[b"XOVER", b"xover", b"Xover"];
    const OVER: &[&[u8]; 3] = &[b"OVER", b"over", b"Over"];
    const XHDR: &[&[u8]; 3] = &[b"XHDR", b"xhdr", b"Xhdr"];
    const HDR: &[&[u8]; 3] = &[b"HDR", b"hdr", b"Hdr"];
    const NEXT: &[&[u8]; 3] = &[b"NEXT", b"next", b"Next"];
    const LAST: &[&[u8]; 3] = &[b"LAST", b"last", b"Last"];
    const LISTGROUP: &[&[u8]; 3] = &[b"LISTGROUP", b"listgroup", b"Listgroup"];
    const POST: &[&[u8]; 3] = &[b"POST", b"post", b"Post"];
    const IHAVE: &[&[u8]; 3] = &[b"IHAVE", b"ihave", b"Ihave"];
    const NEWGROUPS: &[&[u8]; 3] = &[b"NEWGROUPS", b"newgroups", b"Newgroups"];
    const NEWNEWS: &[&[u8]; 3] = &[b"NEWNEWS", b"newnews", b"Newnews"];
    const STARTTLS: &[&[u8]; 3] = &[b"STARTTLS", b"starttls", b"Starttls"];

    if matches_any(cmd, ARTICLE) {
        BenchKind::Article
    } else if matches_any(cmd, BODY) {
        BenchKind::Body
    } else if matches_any(cmd, HEAD) {
        BenchKind::Head
    } else if matches_any(cmd, STAT) {
        BenchKind::Stat
    } else if matches_any(cmd, GROUP) {
        BenchKind::Group
    } else if matches_any(cmd, AUTHINFO) {
        BenchKind::AuthInfo
    } else if matches_any(cmd, LIST) {
        BenchKind::List
    } else if matches_any(cmd, DATE) {
        BenchKind::Date
    } else if matches_any(cmd, MODE) {
        BenchKind::Mode
    } else if matches_any(cmd, HELP) {
        BenchKind::Help
    } else if matches_any(cmd, QUIT) {
        BenchKind::Quit
    } else if matches_any(cmd, CAPABILITIES) {
        BenchKind::Capabilities
    } else if matches_any(cmd, XOVER) {
        BenchKind::Xover
    } else if matches_any(cmd, OVER) {
        BenchKind::Over
    } else if matches_any(cmd, XHDR) {
        BenchKind::Xhdr
    } else if matches_any(cmd, HDR) {
        BenchKind::Hdr
    } else if matches_any(cmd, NEXT) {
        BenchKind::Next
    } else if matches_any(cmd, LAST) {
        BenchKind::Last
    } else if matches_any(cmd, LISTGROUP) {
        BenchKind::ListGroup
    } else if matches_any(cmd, POST) {
        BenchKind::Post
    } else if matches_any(cmd, IHAVE) {
        BenchKind::Ihave
    } else if matches_any(cmd, NEWGROUPS) {
        BenchKind::NewGroups
    } else if matches_any(cmd, NEWNEWS) {
        BenchKind::NewNews
    } else if matches_any(cmd, STARTTLS) {
        BenchKind::StartTls
    } else {
        BenchKind::Unknown
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[inline(never)]
fn classify_workload(verbs: &[&[u8]], classify: impl Fn(&[u8]) -> BenchKind) -> usize {
    let mut acc = 0usize;
    for _ in 0..black_box(1_000usize) {
        for verb in black_box(verbs) {
            acc = black_box(acc ^ black_box(classify(black_box(verb))) as usize);
        }
    }
    black_box(acc)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn main_original_realistic() -> usize {
    classify_workload(black_box(REALISTIC_VERBS), classify_main_original)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn length_bucket_realistic() -> usize {
    classify_workload(black_box(REALISTIC_VERBS), classify_length_bucket)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn main_original_mixed_case() -> usize {
    classify_workload(black_box(MIXED_CASE_VERBS), classify_main_original)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn length_bucket_mixed_case() -> usize {
    classify_workload(black_box(MIXED_CASE_VERBS), classify_length_bucket)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn main_original_all_verbs() -> usize {
    classify_workload(black_box(ALL_VERBS), classify_main_original)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn length_bucket_all_verbs() -> usize {
    classify_workload(black_box(ALL_VERBS), classify_length_bucket)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
library_benchmark_group!(
    name = request_classifier;
    benchmarks =
        main_original_realistic,
        length_bucket_realistic,
        main_original_mixed_case,
        length_bucket_mixed_case,
        main_original_all_verbs,
        length_bucket_all_verbs
);

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
main!(
    config = LibraryBenchmarkConfig::default()
        .tool(Callgrind::with_args(["--instr-atstart=yes"])
            .entry_point(EntryPoint::None));
    library_benchmark_groups = request_classifier
);

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("request_classifier_callgrind is disabled on this target");
}
