//! In-memory backend-to-client streaming benchmarks.
//!
//! These isolate multiline response framing and delivery from TCP socket behavior.

use divan::{Bencher, black_box};
use nntp_proxy::pool::BufferPool;
use nntp_proxy::session::streaming::{StreamContext, stream_multiline_response};
use nntp_proxy::types::{BackendId, BufferSize, ClientAddress};
use std::io::Cursor;
use tokio::runtime::Builder;

fn main() {
    divan::main();
}

const FIRST_CHUNK: &[u8] = b"220 42 <bench@example.com>\r\n";

fn response_tail(body_len: usize) -> Vec<u8> {
    let mut response = Vec::with_capacity(body_len + 5);
    response.extend(std::iter::repeat_n(b'x', body_len));
    response.extend_from_slice(b"\r\n.\r\n");
    response
}

fn stream_context(pool: &BufferPool) -> StreamContext<'_> {
    StreamContext {
        client_addr: ClientAddress::from("127.0.0.1:8000".parse::<std::net::SocketAddr>().unwrap()),
        backend_id: BackendId::from_index(0),
        buffer_pool: pool,
    }
}

fn bench_stream(bencher: Bencher, body_len: usize) {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let pool = BufferPool::new(BufferSize::try_new(64 * 1024).unwrap(), 2);
    let tail = response_tail(body_len);

    bencher
        .counter(divan::counter::BytesCount::new(body_len))
        .bench_local(|| {
            let ctx = stream_context(&pool);
            let mut backend = Cursor::new(tail.as_slice());
            let mut client = tokio::io::sink();
            let bytes = rt
                .block_on(stream_multiline_response(
                    &mut backend,
                    &mut client,
                    FIRST_CHUNK,
                    &ctx,
                ))
                .unwrap();
            black_box(bytes)
        });
}

mod stream_multiline {
    use super::{Bencher, bench_stream};

    #[divan::bench(sample_count = 100, sample_size = 500)]
    fn article_64k(bencher: Bencher) {
        bench_stream(bencher, 64 * 1024);
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn article_768k(bencher: Bencher) {
        bench_stream(bencher, 768 * 1024);
    }
}
