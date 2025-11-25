/// Unit test demonstrating PooledBuffer corruption bug when reused across retries
///
/// This test shows why the buffer MUST be fresh for each retry attempt.
use anyhow::Result;
use nntp_proxy::pool::BufferPool;
use nntp_proxy::types::BufferSize;

#[tokio::test]
async fn test_pooled_buffer_initialized_tracking_bug() -> Result<()> {
    // This test demonstrates the subtle buffer corruption bug.
    //
    // PooledBuffer tracks `initialized` bytes - the number of bytes
    // that were last read into the buffer. The Deref trait returns
    // &self.buffer[..self.initialized].
    //
    // Bug scenario:
    // 1. Buffer reads 100 bytes from Backend1's 430 error
    //    - initialized = 100
    //    - buffer[..100] = "430 error message..."
    //
    // 2. Same buffer reused for Backend2
    //    - read_from() reads 50 bytes
    //    - initialized = 50 (UPDATED)
    //    - buffer[..50] = "220 success..."
    //    - buffer[50..100] = OLD DATA from step 1!
    //
    // 3. If code accesses buffer beyond [..50], corruption!
    //
    // With the fix (fresh buffer per retry), each attempt starts clean.

    let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 10);

    // Simulate first backend returning 430 error (100 bytes)
    let backend1_response = b"430 No such article found on this server, please try another backend for this message\r\n";
    let len1 = backend1_response.len();
    let mut backend1_stream = std::io::Cursor::new(backend1_response);

    {
        let mut buffer = buffer_pool.acquire().await;
        let n1 = buffer.read_from(&mut backend1_stream).await?;
        assert_eq!(n1, len1, "Should read {} bytes from backend1", len1);
        assert_eq!(
            buffer.initialized(),
            len1,
            "Buffer should track {} initialized bytes",
            len1
        );

        // Verify the buffer contains the error
        let data1 = &buffer[..]; // Uses Deref, returns [..initialized]
        assert_eq!(data1.len(), len1);
        assert!(
            std::str::from_utf8(data1)
                .unwrap()
                .contains("No such article")
        );

        // Buffer is dropped here and returned to pool
    }

    // Simulate second backend returning 223 success (shorter - 50 bytes)
    let backend2_response = b"223 0 <test@example.com> Article retrieved OK\r\n";
    let len2 = backend2_response.len();
    let mut backend2_stream = std::io::Cursor::new(backend2_response);

    {
        // In buggy code, this would be THE SAME buffer instance from pool
        let mut buffer = buffer_pool.acquire().await;
        let n2 = buffer.read_from(&mut backend2_stream).await?;
        assert_eq!(n2, len2, "Should read {} bytes from backend2", len2);
        assert_eq!(
            buffer.initialized(),
            len2,
            "Buffer should track {} initialized bytes",
            len2
        );

        // The Deref returns &buffer[..initialized] which is now [..len2]
        let data2 = &buffer[..];
        assert_eq!(data2.len(), len2, "Deref should return {} bytes", len2);

        // This is correct - Deref only returns initialized portion
        assert!(
            !std::str::from_utf8(data2)
                .unwrap()
                .contains("No such article"),
            "Should NOT contain old data via Deref"
        );

        // However, the raw buffer still contains old data beyond `initialized`!
        // This would only be a problem if code accessed it incorrectly.
        let raw_buffer = buffer.as_mut_slice();

        // If we incorrectly accessed beyond the new data size...
        if raw_buffer.len() > len2 {
            // OLD data from backend1 is still there at positions len2..len1!
            // But as long as we use [..n] or Deref, we're safe.
            println!(
                "Raw buffer beyond initialized: {:?}",
                &raw_buffer[len2..len1.min(raw_buffer.len())]
            );
        }
    }

    // The test passes because PooledBuffer's Deref correctly returns
    // only the initialized portion. The fix (fresh buffer) is still
    // important for defense-in-depth and avoiding subtle bugs.

    Ok(())
}

#[tokio::test]
async fn test_buffer_corruption_scenario_with_slice_abuse() -> Result<()> {
    // This demonstrates a hypothetical corruption scenario if code
    // incorrectly uses the buffer slice length from a previous iteration.

    let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 10);

    // First attempt: Read 100 bytes
    let data1 =
        b"430 No such article found on this server - very long error message with lots of text\r\n";
    let mut stream1 = std::io::Cursor::new(data1);

    let mut buffer = buffer_pool.acquire().await;
    let n1 = buffer.read_from(&mut stream1).await?;
    println!("First read: {} bytes", n1);

    // Imagine buggy code that remembers the length
    let remembered_length = n1;

    // In buggy implementation, buffer would be reused...
    // But we can't easily simulate that in a test since the pool
    // is designed correctly. This test documents the concern.

    // Second attempt: Read fewer bytes
    let data2 = b"223 0 <test@example.com>\r\n";
    let mut stream2 = std::io::Cursor::new(data2);

    let n2 = buffer.read_from(&mut stream2).await?;
    println!("Second read: {} bytes", n2);

    // If buggy code used the old length...
    if remembered_length > n2 {
        // And accessed buffer[..remembered_length], it would get corruption!
        // But our code uses buffer[..n2], so it's safe.
        println!(
            "Potential corruption window: bytes {}-{}",
            n2, remembered_length
        );
    }

    Ok(())
}
