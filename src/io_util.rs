use anyhow::{Context, Result};
use std::future::poll_fn;
use std::io::{Error, ErrorKind, IoSlice};
use std::path::Path;
use std::pin::Pin;

pub async fn write_all_vectored<W>(
    writer: &mut W,
    slices: &mut [IoSlice<'_>],
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut remaining = slices;
    while !remaining.is_empty() {
        let written =
            poll_fn(|cx| Pin::new(&mut *writer).poll_write_vectored(cx, remaining)).await?;
        if written == 0 {
            return Err(Error::new(ErrorKind::WriteZero, "failed to write buffers"));
        }
        IoSlice::advance_slices(&mut remaining, written);
    }

    Ok(())
}

pub(crate) fn atomic_replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
    platform::atomic_replace_file(tmp_path, path)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub(super) fn atomic_replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
        let tmp_wide = wide_path(tmp_path);
        let path_wide = wide_path(path);

        // SAFETY: both buffers are valid, null-terminated UTF-16 strings and live
        // for the duration of the call.
        let ok = unsafe {
            MoveFileExW(
                tmp_wide.as_ptr(),
                path_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };

        if ok == 0 {
            Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "Failed to atomically replace {} with {}",
                    path.display(),
                    tmp_path.display()
                )
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs;

    pub(super) fn atomic_replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
        fs::rename(tmp_path, path).with_context(|| {
            format!(
                "Failed to atomically replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })
    }
}

/// Wraps a writer and measures how long writes spend blocked on it.
///
/// Every stretch from a `poll_*` call returning `Pending` to the next call
/// that makes progress is added up. For the client side of an article
/// transfer this is the time a slow downloader held the backend connection,
/// which must not be mistaken for a slow provider. Bytes pass through
/// untouched.
pub(crate) struct StallTimer<W> {
    inner: W,
    blocked_since: Option<std::time::Instant>,
    stalled: std::time::Duration,
}

impl<W> StallTimer<W> {
    pub(crate) const fn new(inner: W) -> Self {
        Self {
            inner,
            blocked_since: None,
            stalled: std::time::Duration::ZERO,
        }
    }

    /// Total time spent blocked so far.
    pub(crate) fn stalled(&self) -> std::time::Duration {
        self.stalled
            + self
                .blocked_since
                .map_or(std::time::Duration::ZERO, |since| since.elapsed())
    }

    fn observe<T>(&mut self, poll: std::task::Poll<T>) -> std::task::Poll<T> {
        match poll {
            std::task::Poll::Pending => {
                if self.blocked_since.is_none() {
                    self.blocked_since = Some(std::time::Instant::now());
                }
            }
            std::task::Poll::Ready(_) => {
                if let Some(since) = self.blocked_since.take() {
                    self.stalled += since.elapsed();
                }
            }
        }
        poll
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for StallTimer<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.observe(poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.observe(poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        self.observe(poll)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod stall_timer_tests {
    use super::StallTimer;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn counts_time_blocked_on_a_slow_reader_and_passes_bytes_through() {
        // A 4-byte pipe: the second write blocks until the reader drains.
        let (client, mut reader) = tokio::io::duplex(4);
        let mut timer = StallTimer::new(client);
        let drain = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let mut out = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut out)
                .await
                .unwrap();
            out
        });
        timer.write_all(b"abcdefgh").await.unwrap();
        timer.shutdown().await.unwrap();
        assert!(timer.stalled() >= std::time::Duration::from_millis(40));
        drop(timer);
        assert_eq!(drain.await.unwrap(), b"abcdefgh");
    }

    #[tokio::test]
    async fn a_fast_writer_reports_no_stall() {
        let mut timer = StallTimer::new(Vec::new());
        timer.write_all(b"payload").await.unwrap();
        assert_eq!(timer.stalled(), std::time::Duration::ZERO);
    }
}
