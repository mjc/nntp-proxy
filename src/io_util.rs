use std::future::poll_fn;
use std::io::{Error, ErrorKind, IoSlice};
use std::pin::Pin;

pub(crate) async fn write_all_vectored<W>(
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
