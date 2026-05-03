use anyhow::{Context, Result};
use std::future::poll_fn;
use std::io::{Error, ErrorKind, IoSlice};
use std::pin::Pin;
use std::{fs, path::Path};

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

#[cfg(unix)]
pub(crate) fn atomic_replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
    fs::rename(tmp_path, path).with_context(|| {
        format!(
            "Failed to atomically replace {} with {}",
            path.display(),
            tmp_path.display()
        )
    })
}

#[cfg(windows)]
pub(crate) fn atomic_replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
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
