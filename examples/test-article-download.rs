//! Test tool to download a specific article and diagnose transfer issues
//!
//! Usage: test-article-download <host:port> <NNTP-command> [username] [password]

use std::env;
use std::io::Write;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse arguments
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <host:port> <NNTP-command> [username] [password]",
            args[0]
        );
        eprintln!("\nExamples:");
        eprintln!("  {} localhost:8119 'BODY <msg@example.com>'", args[0]);
        eprintln!(
            "  {} localhost:8119 'ARTICLE <msg@example.com>' user pass",
            args[0]
        );
        std::process::exit(1);
    }

    let server_addr = &args[1];
    let command_input = &args[2];
    let username = args.get(3).map(String::as_str);
    let password = args.get(4).map(String::as_str);

    // Validate the command looks like an NNTP command
    let command_upper = command_input.to_uppercase();
    if !command_upper.starts_with("BODY ")
        && !command_upper.starts_with("ARTICLE ")
        && !command_upper.starts_with("HEAD ")
    {
        eprintln!("Error: Command must start with BODY, ARTICLE, or HEAD");
        eprintln!("Got: {}", command_input);
        std::process::exit(1);
    }

    println!("Connecting to: {}", server_addr);
    println!("Command: {}", command_input);
    println!();

    // Connect
    let mut stream = TcpStream::connect(server_addr).await?;
    println!("✓ Connected");

    // Read greeting
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let greeting = String::from_utf8_lossy(&buf[..n]);
    println!("← Greeting: {}", greeting.trim());

    // Authenticate if credentials provided
    if let (Some(user), Some(pass)) = (username, password) {
        println!("\nAuthenticating as '{}'...", user);

        // Send AUTHINFO USER
        let user_cmd = format!("AUTHINFO USER {}\r\n", user);
        stream.write_all(user_cmd.as_bytes()).await?;
        stream.flush().await?;
        println!("→ AUTHINFO USER {}", user);

        let n = stream.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]);
        println!("← {}", response.trim());

        // Send AUTHINFO PASS
        let pass_cmd = format!("AUTHINFO PASS {}\r\n", pass);
        stream.write_all(pass_cmd.as_bytes()).await?;
        stream.flush().await?;
        println!("→ AUTHINFO PASS ****");

        let n = stream.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]);
        println!("← {}", response.trim());

        if !response.starts_with("281") {
            anyhow::bail!("Authentication failed: {}", response.trim());
        }
        println!("✓ Authenticated\n");
    }

    // Send the command
    let nntp_cmd = format!("{}\r\n", command_input);
    println!("Sending command...");
    println!("→ {}", command_input);

    stream.write_all(nntp_cmd.as_bytes()).await?;
    stream.flush().await?;

    // Read status line
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    println!("← Status: {}", status_line.trim());

    // Check if it's a multiline response (222)
    if !status_line.starts_with("222") {
        println!("\n✗ Article not available or error response");
        return Ok(());
    }

    println!("\n✓ Article found, downloading...\n");

    // Download the body with detailed progress
    let mut total_bytes = 0u64;
    let mut chunk_count = 0u64;
    let mut buffer = vec![0u8; 64 * 1024]; // 64KB chunks
    let mut last_report = 0u64;
    let report_interval = 100 * 1024; // Report every 100KB

    let start_time = std::time::Instant::now();

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                println!("\n✗ Connection closed unexpectedly");
                println!(
                    "Total downloaded: {} bytes ({:.2} MB)",
                    total_bytes,
                    total_bytes as f64 / 1_048_576.0
                );
                println!("Expected terminator (\\r\\n.\\r\\n) not found");
                break;
            }
            Ok(n) => {
                chunk_count += 1;
                total_bytes += n as u64;

                // Check for terminator in this chunk
                if buffer[..n].windows(5).any(|w| w == b"\r\n.\r\n") {
                    println!("\n✓ Found terminator!");
                    println!(
                        "Total downloaded: {} bytes ({:.2} MB)",
                        total_bytes,
                        total_bytes as f64 / 1_048_576.0
                    );
                    println!("Chunks received: {}", chunk_count);
                    println!("Transfer time: {:?}", start_time.elapsed());
                    println!(
                        "Average speed: {:.2} MB/s",
                        (total_bytes as f64 / 1_048_576.0) / start_time.elapsed().as_secs_f64()
                    );
                    break;
                }

                // Progress reporting
                if total_bytes - last_report >= report_interval {
                    let mb = total_bytes as f64 / 1_048_576.0;
                    let speed =
                        (total_bytes as f64 / 1_048_576.0) / start_time.elapsed().as_secs_f64();
                    print!(
                        "\rProgress: {:.2} MB ({} chunks, {:.2} MB/s)",
                        mb, chunk_count, speed
                    );
                    std::io::stdout().flush()?;
                    last_report = total_bytes;
                }

                // Special attention around 2.27 MB (the problematic size)
                let mb = total_bytes as f64 / 1_048_576.0;
                if mb >= 2.26 && mb <= 2.28 {
                    println!("\n⚠ Entering critical zone: {:.3} MB", mb);
                }
            }
            Err(e) => {
                println!("\n✗ Error reading from connection: {}", e);
                println!("Error kind: {:?}", e.kind());
                println!(
                    "Total downloaded before error: {} bytes ({:.2} MB)",
                    total_bytes,
                    total_bytes as f64 / 1_048_576.0
                );
                println!("Chunks received: {}", chunk_count);
                return Err(e.into());
            }
        }
    }

    println!("\n✓ Download complete");
    Ok(())
}
