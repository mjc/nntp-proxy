//! Gungraun instruction-count benchmarks for the proxy multiline framer.
//!
//! The benchmark keeps the input immutable and compares the production
//! incremental operation with the stateless control at representative read
//! sizes.  It is intentionally separate from the Divan timing benchmark so
//! instruction deltas are reproducible under Callgrind.

macro_rules! supported {
    ($($item:item)*) => {
        $(
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            $item
        )*
    };
}

supported! {
    use gungraun::{
        Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group,
        main,
    };
    use nntp_proxy::session::{
        benchmark_incremental_multiline_frame, benchmark_multiline_response,
        benchmark_stateless_multiline_frame,
    };
    use std::hint::black_box;
    use std::sync::LazyLock;

    static RESPONSE_64K: LazyLock<Vec<u8>> =
        LazyLock::new(|| benchmark_multiline_response(64 * 1024));
    static RESPONSE_768K: LazyLock<Vec<u8>> =
        LazyLock::new(|| benchmark_multiline_response(768 * 1024));

    #[inline(never)]
    fn incremental_64k_one_byte() -> usize {
        benchmark_incremental_multiline_frame(black_box(&RESPONSE_64K), 1)
    }

    #[inline(never)]
    fn incremental_64k_whole_read() -> usize {
        benchmark_incremental_multiline_frame(black_box(&RESPONSE_64K), usize::MAX)
    }

    #[inline(never)]
    fn incremental_768k_thirty_one_bytes() -> usize {
        benchmark_incremental_multiline_frame(black_box(&RESPONSE_768K), 31)
    }

    #[inline(never)]
    fn stateless_64k_one_byte() -> usize {
        benchmark_stateless_multiline_frame(black_box(&RESPONSE_64K), 1)
    }

    #[inline(never)]
    fn stateless_64k_whole_read() -> usize {
        benchmark_stateless_multiline_frame(black_box(&RESPONSE_64K), usize::MAX)
    }

    #[library_benchmark]
    fn bench_incremental_64k_one_byte() -> usize {
        incremental_64k_one_byte()
    }

    #[library_benchmark]
    fn bench_incremental_64k_whole_read() -> usize {
        incremental_64k_whole_read()
    }

    #[library_benchmark]
    fn bench_incremental_768k_thirty_one_bytes() -> usize {
        incremental_768k_thirty_one_bytes()
    }

    #[library_benchmark]
    fn bench_stateless_64k_one_byte() -> usize {
        stateless_64k_one_byte()
    }

    #[library_benchmark]
    fn bench_stateless_64k_whole_read() -> usize {
        stateless_64k_whole_read()
    }

    library_benchmark_group!(
        name = multiline_framing;
        benchmarks =
            bench_incremental_64k_one_byte,
            bench_incremental_64k_whole_read,
            bench_incremental_768k_thirty_one_bytes,
            bench_stateless_64k_one_byte,
            bench_stateless_64k_whole_read
    );

    main!(
        config = LibraryBenchmarkConfig::default()
            .tool(Callgrind::with_args(["--instr-atstart=yes"])
                .entry_point(EntryPoint::None));
        library_benchmark_groups = multiline_framing
    );
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("multiline_framing_callgrind is disabled on this target");
}
