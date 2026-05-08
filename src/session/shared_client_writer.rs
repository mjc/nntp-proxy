use std::sync::Arc;

/// Shareable client writer used by per-command routing and backend workers.
#[derive(Clone, Debug)]
pub(crate) struct SharedClientWriter {
    inner: Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
}

impl SharedClientWriter {
    #[must_use]
    pub(crate) fn new(write_half: tokio::net::tcp::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(write_half)),
        }
    }

    pub(crate) async fn lock(
        &self,
    ) -> tokio::sync::MutexGuard<'_, tokio::net::tcp::OwnedWriteHalf> {
        self.inner.lock().await
    }

    pub(crate) fn try_into_inner(self) -> Result<tokio::net::tcp::OwnedWriteHalf, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(mutex) => Ok(mutex.into_inner()),
            Err(inner) => Err(Self { inner }),
        }
    }
}
