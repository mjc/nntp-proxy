use crate::pool::BufferPool;
use anyhow::Result;
use tokio::io::AsyncWriteExt;

pub(super) async fn read_response<R>(
    backend_read: &mut R,
    first_chunk: &[u8],
    first_chunk_size: usize,
    is_multiline: bool,
    buffer_pool: &BufferPool,
) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut response = Vec::with_capacity(first_chunk_size.saturating_add(128));
    response.extend_from_slice(&first_chunk[..first_chunk_size]);

    if !is_multiline || crate::protocol::has_multiline_terminator(&response) {
        return Ok(response);
    }

    let mut buffer = buffer_pool.acquire().await;
    loop {
        let n = buffer.read_from(backend_read).await?;
        if n == 0 {
            anyhow::bail!("Backend EOF before CAPABILITIES terminator");
        }

        response.extend_from_slice(&buffer[..n]);
        if crate::protocol::has_multiline_terminator(&response) {
            return Ok(response);
        }
    }
}

pub(super) fn rewrite_response(response: &[u8], is_authenticated: bool) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(response) else {
        return response.to_vec();
    };

    let mut lines = text.split_inclusive("\r\n");
    let Some(status_line) = lines.next() else {
        return response.to_vec();
    };

    if !status_line.starts_with("101") {
        return response.to_vec();
    }

    let mut rewritten = String::with_capacity(text.len() + 16);
    rewritten.push_str(status_line);

    for line in lines {
        if line == ".\r\n" {
            if !is_authenticated {
                rewritten.push_str("AUTHINFO USER\r\n");
            }
            rewritten.push_str(line);
            return rewritten.into_bytes();
        }

        let capability = line.trim_end_matches("\r\n");
        let keyword = capability.split_ascii_whitespace().next().unwrap_or("");

        if keyword.eq_ignore_ascii_case("AUTHINFO") || keyword.eq_ignore_ascii_case("SASL") {
            continue;
        }

        if capability.eq_ignore_ascii_case("MODE-READER") {
            continue;
        }

        rewritten.push_str(line);
    }

    response.to_vec()
}

pub(super) async fn proxy_command<BR, BW, CW>(
    backend_read: &mut BR,
    backend_write: &mut BW,
    client_write: &mut CW,
    command: &str,
    buffer_pool: &BufferPool,
    is_authenticated: bool,
) -> Result<u64>
where
    BR: tokio::io::AsyncRead + Unpin,
    BW: tokio::io::AsyncWrite + Unpin,
    CW: tokio::io::AsyncWrite + Unpin,
{
    backend_write.write_all(command.as_bytes()).await?;

    let mut first_chunk = buffer_pool.acquire().await;
    let first_n = first_chunk.read_from(backend_read).await?;
    if first_n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    let validated = crate::session::backend::validate_backend_response(
        &first_chunk[..first_n],
        first_n,
        crate::protocol::MIN_RESPONSE_LENGTH,
    );
    let response = read_response(
        backend_read,
        &first_chunk[..first_n],
        first_n,
        validated.is_multiline,
        buffer_pool,
    )
    .await?;
    let rewritten = rewrite_response(&response, is_authenticated);

    client_write.write_all(&rewritten).await?;
    Ok(rewritten.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::rewrite_response;

    #[test]
    fn test_rewrite_capability_response_before_authentication() {
        let response = b"101 Capability list\r\nVERSION 2\r\nREADER\r\nMODE-READER\r\nAUTHINFO SASL\r\nSASL PLAIN\r\n.\r\n";

        let rewritten = String::from_utf8(rewrite_response(response, false)).unwrap();

        assert!(rewritten.contains("\r\nAUTHINFO USER\r\n"));
        assert!(!rewritten.contains("AUTHINFO SASL"));
        assert!(!rewritten.contains("\r\nSASL "));
        assert!(!rewritten.contains("\r\nMODE-READER\r\n"));
        assert!(rewritten.contains("\r\nREADER\r\n"));
    }

    #[test]
    fn test_rewrite_capability_response_after_authentication() {
        let response = b"101 Capability list\r\nVERSION 2\r\nREADER\r\nMODE-READER\r\nAUTHINFO USER SASL\r\nSASL PLAIN\r\n.\r\n";

        let rewritten = String::from_utf8(rewrite_response(response, true)).unwrap();

        assert!(!rewritten.contains("\r\nAUTHINFO"));
        assert!(!rewritten.contains("\r\nSASL "));
        assert!(!rewritten.contains("\r\nMODE-READER\r\n"));
        assert!(rewritten.contains("\r\nREADER\r\n"));
    }

    #[test]
    fn test_rewrite_capability_response_preserves_non_capabilities_response() {
        let response = b"500 What?\r\n";
        assert_eq!(rewrite_response(response, false), response);
    }
}
