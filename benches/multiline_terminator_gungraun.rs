//! Instruction counts for the same three scanner loops as Divan.
//! This does not measure RAM latency. Fixture creation and dispatch warmup
//! are excluded through Gungraun's setup argument.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod supported {
    use gungraun::{library_benchmark, library_benchmark_group};
    use memchr::memmem::Finder;
    use nntp_proxy::session::multiline_framing_bench::{
        finder, packed_stream, scan_ashwa, scan_finder, scan_memchr, validate,
    };
    use std::{hint::black_box, sync::OnceLock};

    fn setup() -> &'static (Vec<u8>, Finder<'static>) {
        static INPUT: OnceLock<(Vec<u8>, Finder<'static>)> = OnceLock::new();
        INPUT.get_or_init(|| {
            let data = packed_stream();
            let finder = finder();
            validate(&data, &finder);
            (data, finder)
        })
    }

    #[library_benchmark]
    #[bench::packed(setup())]
    fn memchr_convenience_packed_8m(input: &(Vec<u8>, Finder<'_>)) -> (usize, usize) {
        black_box(scan_memchr(black_box(&input.0)))
    }

    #[library_benchmark]
    #[bench::packed(setup())]
    fn memchr_finder_packed_8m(input: &(Vec<u8>, Finder<'_>)) -> (usize, usize) {
        black_box(scan_finder(black_box(&input.0), black_box(&input.1)))
    }

    #[library_benchmark]
    #[bench::packed(setup())]
    fn ashwa_packed_8m(input: &(Vec<u8>, Finder<'_>)) -> (usize, usize) {
        black_box(scan_ashwa(black_box(&input.0)))
    }

    library_benchmark_group!(
        name = multiline_terminator;
        benchmarks = memchr_convenience_packed_8m, memchr_finder_packed_8m, ashwa_packed_8m
    );
    gungraun::main!(library_benchmark_groups = multiline_terminator);

    pub fn run() {
        main();
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() {
    supported::run();
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    panic!("Gungraun requires a supported Linux target");
}
