//! Direct public-client STAT benchmark.
//!
//! The fixture is created outside the timed closure. Each iteration exercises
//! the real `NntpClient::stat` receive/observe/reuse path over a persistent
//! local NNTP connection.

use divan::{Bencher, black_box};
use nntp_proxy::client::NntpClient;
use nntp_proxy::pool::{BufferPool, DeadpoolConnectionProvider};
use nntp_proxy::protocol::{RequestContext, RequestKind};
use nntp_proxy::types::{BufferSize, MessageId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};

const STAT_RESPONSE: &[u8] = b"223 42 <bench@example.com> exists\r\n";

struct Fixture {
    runtime: Runtime,
    client: NntpClient,
    message_id: MessageId<'static>,
}

impl Fixture {
    fn new() -> Self {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let addr = runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    tokio::spawn(async move {
                        let (read_half, mut write_half) = stream.into_split();
                        if write_half
                            .write_all(b"200 benchmark ready\r\n")
                            .await
                            .is_err()
                        {
                            return;
                        }
                        let mut reader = BufReader::new(read_half);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            let Ok(read) = reader.read_line(&mut line).await else {
                                return;
                            };
                            if read == 0 {
                                return;
                            }
                            let kind = RequestContext::parse(line.as_bytes())
                                .map(|request| request.kind());
                            let reply = match kind {
                                Some(RequestKind::Stat) => STAT_RESPONSE,
                                Some(RequestKind::Quit) => {
                                    let _ = write_half.write_all(b"205 closing\r\n").await;
                                    return;
                                }
                                _ => b"200 OK\r\n",
                            };
                            if write_half.write_all(reply).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            addr
        });

        let provider = DeadpoolConnectionProvider::builder(addr.ip().to_string(), addr.port())
            .name("client-stat-bench")
            .max_connections(1)
            .build()
            .unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 1);
        Self {
            runtime,
            client: NntpClient::new(provider, buffer_pool),
            message_id: MessageId::new("<bench@example.com>".to_owned()).unwrap(),
        }
    }
}

#[divan::bench(sample_count = 50, sample_size = 100)]
fn stat(bencher: Bencher) {
    let fixture = Fixture::new();
    bencher.bench_local(|| {
        let result = fixture
            .runtime
            .block_on(fixture.client.stat(&fixture.message_id))
            .unwrap();
        black_box(result)
    });
}

fn main() {
    divan::main();
}
