/// Client writer owned by one per-command session.
#[derive(Debug)]
pub(crate) struct ClientWriter {
    inner: tokio::net::tcp::OwnedWriteHalf,
}

impl ClientWriter {
    #[must_use]
    pub(crate) fn new(write_half: tokio::net::tcp::OwnedWriteHalf) -> Self {
        Self { inner: write_half }
    }

    pub(crate) fn get_mut(&mut self) -> &mut tokio::net::tcp::OwnedWriteHalf {
        &mut self.inner
    }

    pub(crate) fn into_inner(self) -> tokio::net::tcp::OwnedWriteHalf {
        self.inner
    }
}

/// Lets shared routing code write to either a per-command session's writer or
/// a stateful loop's raw client half.
impl tokio::io::AsyncWrite for ClientWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, buffers)
    }
}
