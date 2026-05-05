//! RFC 4643 compliance bug tests
//!
//! Tests for the three AUTHINFO compliance bugs identified in the audit:
//! - Bug 1 (§2.3.2): AUTHINFO PASS without prior USER must return 482
//! - Bug 2 (§2.2): AUTHINFO after successful auth must return 502
//! - Bug 3 (§2.3.1): Unknown AUTHINFO subcommand must return 501

use anyhow::Result;

use crate::test_helpers::{MockNntpServer, RfcTestClient};
use nntp_proxy::config::RoutingMode;

async fn spawn_auth_client(
    mode: RoutingMode,
    username: &str,
    password: &str,
) -> Result<RfcTestClient> {
    RfcTestClient::spawn_with_auth(mode, "auth-backend", username, password, |port| {
        MockNntpServer::new(port)
    })
    .await
}

// ============================================================================
// Bug 1: AUTHINFO PASS without prior USER → 482 (§2.3.2)
// ============================================================================

/// RFC 4643 §2.3.2: "AUTHINFO PASS command MUST NOT be issued before a
/// successful AUTHINFO USER command … the server MUST respond with response
/// code 482."
#[tokio::test]
async fn test_authinfo_pass_without_user_returns_482() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::PerCommand, "testuser", "testpass").await?;
    client
        .expect_status("AUTHINFO PASS testpass", "482")
        .await?;
    Ok(())
}

/// Same as above but in Stateful routing mode to confirm the fix applies there too.
#[tokio::test]
async fn test_authinfo_pass_without_user_returns_482_stateful() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::Stateful, "testuser", "testpass").await?;
    client
        .expect_status("AUTHINFO PASS testpass", "482")
        .await?;
    Ok(())
}

// ============================================================================
// Bug 2: AUTHINFO after successful authentication → 502 (§2.2)
// ============================================================================

/// RFC 4643 §2.2: "Once a client has successfully authenticated … any
/// subsequent AUTHINFO commands … MUST be rejected with a 502 response."
#[tokio::test]
async fn test_authinfo_after_auth_returns_502_per_command() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::PerCommand, "alice", "wonderland").await?;
    client.authenticate("alice", "wonderland").await?;
    client.expect_status("AUTHINFO USER alice", "502").await?;
    Ok(())
}

/// Same check in Stateful routing mode — the hot-path forwarding must also be intercepted.
#[tokio::test]
async fn test_authinfo_after_auth_returns_502_stateful() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::Stateful, "alice", "wonderland").await?;
    client.authenticate("alice", "wonderland").await?;
    client.expect_status("AUTHINFO USER alice", "502").await?;
    Ok(())
}

// ============================================================================
// Bug 3: Unknown AUTHINFO subcommand → 501 (§2.3.1)
// ============================================================================

/// RFC 4643 §2.2 + §2.3.1: After authentication, even an unknown AUTHINFO subcommand
/// must return 502 (already authenticated) rather than 501 (syntax error). The §2.2
/// rule that all AUTHINFO commands must be rejected post-auth takes precedence.
#[tokio::test]
async fn test_authinfo_unknown_subcommand_returns_502_when_authenticated() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::PerCommand, "alice", "wonderland").await?;
    client.authenticate("alice", "wonderland").await?;
    client
        .expect_status("AUTHINFO GENERIC some-mechanism", "502")
        .await?;
    Ok(())
}

/// Unknown AUTHINFO subcommand before authentication should also return 501,
/// not 480 (auth-required).
#[tokio::test]
async fn test_authinfo_unknown_subcommand_returns_501_unauthenticated() -> Result<()> {
    let mut client = spawn_auth_client(RoutingMode::PerCommand, "alice", "wonderland").await?;
    client
        .expect_status("AUTHINFO GENERIC some-mechanism", "501")
        .await?;
    Ok(())
}
