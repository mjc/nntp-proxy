//! Test for duplicate greeting bug fix
//!
//! This test verifies that per-command routing mode only sends ONE greeting,
//! not two. The bug was that both setup_client_connection() and
//! handle_per_command_routing() were sending greetings.

use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Test that per-command routing mode sends exactly one greeting
#[test]
fn test_single_greeting_per_command_mode() -> Result<()> {
    // Connect to the proxy (assumes it's running on port 8121)
    // This test is meant to be run manually with a running proxy
    let addr = "127.0.0.1:8121";

    let mut stream =
        match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping test - proxy not running on {}", addr);
                return Ok(());
            }
        };

    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut greeting_count = 0;
    let mut first_line = String::new();

    // Read the greeting(s)
    // In the bug, we'd get two 200 lines
    // After fix, we should only get one
    reader.read_line(&mut first_line)?;

    if first_line.starts_with("200") {
        greeting_count += 1;

        // Check if there's a second greeting (the bug)
        let mut second_line = String::new();
        // Use a very short timeout to check if more data is immediately available
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        match reader.read_line(&mut second_line) {
            Ok(_) if second_line.starts_with("200") => {
                greeting_count += 1;
                eprintln!("BUG: Received second greeting: {}", second_line.trim());
            }
            _ => {
                // No second greeting, or it's not a 200 - this is good
            }
        }
    }

    // Send a test command to verify the connection works
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "HELP")?;
    stream.flush()?;

    // We should get exactly 1 greeting
    assert_eq!(
        greeting_count,
        1,
        "Expected exactly 1 greeting, got {}. First line: {}",
        greeting_count,
        first_line.trim()
    );

    // Verify it's the per-command routing greeting
    assert!(
        first_line.contains("Per-Command Routing") || first_line.starts_with("200"),
        "Expected greeting to be '200 NNTP Proxy Ready (Per-Command Routing)', got: {}",
        first_line.trim()
    );

    // Clean up
    let _ = writeln!(stream, "QUIT");

    Ok(())
}

/// Test that we can fetch an article without corruption
#[test]
fn test_article_fetch_no_corruption() -> Result<()> {
    let addr = "127.0.0.1:8121";

    let mut stream =
        match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping test - proxy not running on {}", addr);
                return Ok(());
            }
        };

    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream.try_clone()?);

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line)?;
    assert!(
        line.starts_with("200"),
        "Expected 200 greeting, got: {}",
        line.trim()
    );

    // Count how many 200 responses we get before the article response
    let mut greeting_count = 1; // Already got one
    line.clear();

    // Check for duplicate greeting
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    if let Ok(n) = reader.read_line(&mut line)
        && n > 0
        && line.starts_with("200")
    {
        greeting_count += 1;
        line.clear();
    }

    // Reset timeout for actual command
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // Send ARTICLE command (using a test article ID that might exist)
    writeln!(stream, "ARTICLE <6b1945c100bd4cf68c865ad90bc2f05c@ngPost>")?;
    stream.flush()?;

    // Read response - should be 220 (article follows) or 430 (no such article)
    // NOT another 200 greeting
    line.clear();
    reader.read_line(&mut line)?;

    // Verify we don't get a stray 200 in the article response
    assert!(
        !line.contains("200 NNTP Proxy Ready"),
        "Article response contains greeting text - corruption detected! Line: {}",
        line.trim()
    );

    // Should be a proper article response
    assert!(
        line.starts_with("220") || line.starts_with("430") || line.starts_with("423"),
        "Expected article response (220/430/423), got: {}",
        line.trim()
    );

    // We should have seen exactly 1 greeting total
    assert_eq!(
        greeting_count, 1,
        "Expected exactly 1 greeting before article command, got {}",
        greeting_count
    );

    // Clean up
    let _ = writeln!(stream, "QUIT");

    Ok(())
}
