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
