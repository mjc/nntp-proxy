//! Callgrind benchmarks for NNTP request verb classification.
//!
//! The `request_line_*` benches exercise the current borrowed request-line parser.
//!
//! Run with: `cargo bench --bench request_classifier_callgrind`

macro_rules! supported {
    ($($item:item)*) => {
        $(
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            $item
        )*
    };
}

supported! {
    use iai_callgrind::{
        Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
    };
    use nntp_proxy::protocol::{RequestKind, RequestLine};
    use std::hint::black_box;

    const REALISTIC_VERBS: &[&[u8]] = &[
        b"ARTICLE", b"BODY", b"HEAD", b"STAT", b"ARTICLE", b"BODY", b"HEAD", b"GROUP",
        b"OVER", b"LIST", b"DATE", b"CAPABILITIES", b"QUIT",
    ];
    const MIXED_CASE_VERBS: &[&[u8]] = &[
        b"Article", b"body", b"HeAd", b"stat", b"gRoUp", b"listgroup", b"Capabilities",
        b"authinfo", b"StartTls",
    ];
    const ALL_VERBS: &[&[u8]] = &[
        b"ARTICLE", b"BODY", b"HEAD", b"STAT", b"GROUP", b"LISTGROUP", b"LAST", b"NEXT",
        b"LIST", b"DATE", b"HELP", b"CAPABILITIES", b"MODE", b"QUIT", b"OVER", b"XOVER",
        b"HDR", b"XHDR", b"NEWGROUPS", b"NEWNEWS", b"POST", b"IHAVE", b"AUTHINFO",
        b"STARTTLS", b"XUNKNOWN",
    ];
    #[inline(never)]
    fn classify_request_line(verb: &[u8]) -> RequestKind {
        RequestLine::parse(verb).kind()
    }

    #[inline(never)]
    fn classify_workload(verbs: &[&[u8]], classify: impl Fn(&[u8]) -> RequestKind) -> usize {
        (0..black_box(1_000usize)).fold(0usize, |acc, _| {
            black_box(verbs).iter().fold(acc, |acc, verb| {
                black_box(acc ^ black_box(classify(black_box(verb))) as usize)
            })
        })
    }

    macro_rules! bench_classifier {
        ($name:ident, $verbs:ident) => {
            #[library_benchmark]
            fn $name() -> usize {
                classify_workload(black_box($verbs), classify_request_line)
            }
        };
    }

    bench_classifier!(request_line_realistic, REALISTIC_VERBS);
    bench_classifier!(request_line_mixed_case, MIXED_CASE_VERBS);
    bench_classifier!(request_line_all_verbs, ALL_VERBS);

    library_benchmark_group!(
        name = request_classifier;
        benchmarks =
            request_line_realistic,
            request_line_mixed_case,
            request_line_all_verbs
    );

    main!(
        config = LibraryBenchmarkConfig::default()
            .tool(Callgrind::with_args(["--instr-atstart=yes"])
                .entry_point(EntryPoint::None));
        library_benchmark_groups = request_classifier
    );
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("request_classifier_callgrind is disabled on this target");
}
