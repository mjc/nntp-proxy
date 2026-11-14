//! Connection error types for the NNTP proxy

use std::error::Error as StdError;
use std::io::ErrorKind;
use thiserror::Error;

/// Error kinds that indicate the client disconnected
///
/// These errors should not be logged as backend failures - they're normal
/// when clients close connections.
pub const DISCONNECT_KINDS: &[ErrorKind] = &[ErrorKind::BrokenPipe, ErrorKind::ConnectionReset];

/// Error kinds that indicate the connection is broken and should not be reused
///
/// When these errors occur, the connection should be removed from the pool
/// rather than returned for reuse.
pub const CONNECTION_ERROR_KINDS: &[ErrorKind] = &[
    ErrorKind::BrokenPipe,
    ErrorKind::ConnectionReset,
    ErrorKind::ConnectionAborted,
    ErrorKind::UnexpectedEof,
];

/// Check if an I/O error kind indicates a client disconnect
#[inline]
#[must_use]
pub fn is_disconnect_kind(kind: ErrorKind) -> bool {
    DISCONNECT_KINDS.contains(&kind)
}

/// Check if an I/O error kind indicates a broken connection
#[inline]
#[must_use]
pub fn is_connection_error_kind(kind: ErrorKind) -> bool {
    CONNECTION_ERROR_KINDS.contains(&kind)
}

/// Errors that can occur during connection management
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectionError {
    #[error("Authentication failed for backend '{backend}': {response}")]
    AuthenticationFailed { backend: String, response: String },

    #[error("Connection limit exceeded for backend '{backend}': {response}")]
    ConnectionLimitExceeded { backend: String, response: String },

    #[error("Backend '{backend}' refused a recent login; not retrying for {retry_in:?}")]
    LoginBackoff {
        backend: String,
        retry_in: std::time::Duration,
    },

    #[error("Invalid greeting from backend '{backend}': {greeting}")]
    InvalidGreeting { backend: String, greeting: String },

    /// `MODE READER` refused after a successful login.
    #[error("Backend '{backend}' refused MODE READER: {response}")]
    ReaderModeRefused { backend: String, response: String },

    #[error("Connection pool exhausted for backend '{backend}' (max size: {max_size})")]
    PoolExhausted { backend: String, max_size: usize },

    /// The pool's queue is too long to serve this checkout within its wait,
    /// so the checkout did not join it.
    #[error("Backend '{backend}' is backlogged (estimated wait {estimated_wait:?})")]
    Backlogged {
        backend: std::sync::Arc<str>,
        estimated_wait: std::time::Duration,
    },

    #[error("Connection pool for backend '{backend}' is closed")]
    PoolClosed { backend: String },

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("TLS handshake failed for backend '{backend}': {source}")]
    TlsHandshake {
        backend: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("No DNS addresses found for {address}")]
    DnsNoAddresses { address: String },

    #[error("Failed to initialize DNS resolver for backend '{backend}': {source}")]
    DnsResolverInit {
        backend: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    #[error("Failed to build DNS resolver for backend '{backend}': {source}")]
    DnsResolverBuild {
        backend: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    #[error("Failed to resolve {address} for backend '{backend}': {source}")]
    DnsLookup {
        backend: String,
        address: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    #[error("Password required but not configured for backend '{backend}'")]
    PasswordRequired { backend: String },

    #[error("Unexpected auth response from backend '{backend}': {response}")]
    UnexpectedAuthResponse { backend: String, response: String },

    #[error("Compression required but not supported by backend '{backend}': {response}")]
    CompressionRequired { backend: String, response: String },
}

/// Why a backend that answered would not let a session in.
///
/// The single classification of refused logins: login backoff applies to
/// every kind, but only [`LoginRefusal::ConnectionLimit`] says anything about
/// how many connections the account allows, so only it may lower an adaptive
/// connection cap. A bad password or a locked account would otherwise shrink
/// the pool for a problem that more or fewer connections cannot fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginRefusal {
    /// `481` to `AUTHINFO`: bad credentials, whatever the text says.
    Credentials,
    /// The account is at its connection limit: see
    /// [`is_connection_limit_reply`].
    ConnectionLimit,
    /// Service refused before login: a non-2xx greeting without
    /// connection-limit wording.
    ServiceUnavailable,
    /// `MODE READER` refused after a successful login.
    ReaderModeRefused,
    /// Any other refusal at `AUTHINFO`; the reply text says why.
    Rejected,
}

/// What a refused `AUTHINFO USER` or `AUTHINFO PASS` reply means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthinfoReply {
    /// `481`: bad credentials, whatever the text says.
    BadCredentials,
    /// See [`is_connection_limit_reply`].
    ConnectionLimit,
    /// Any other refusal.
    Other,
}

/// Classify a refused `AUTHINFO` reply. The one rule for both `AUTHINFO`
/// steps, used both to construct the error and to classify the refusal, so
/// the error variant never contradicts the classification.
#[must_use]
pub fn classify_authinfo_reply(reply: &str) -> AuthinfoReply {
    if is_bad_credentials_code(reply) {
        AuthinfoReply::BadCredentials
    } else if is_connection_limit_reply(reply) {
        AuthinfoReply::ConnectionLimit
    } else {
        AuthinfoReply::Other
    }
}

/// `481` always means bad credentials, whatever its text says.
fn is_bad_credentials_code(reply: &str) -> bool {
    crate::protocol::StatusCode::parse(reply.as_bytes()).is_some_and(|code| code.as_u16() == 481)
}

/// Nouns a connection-limit phrase ends with.
const CONNECTION_NOUNS: &[&str] = &["connections", "connection", "conns", "sessions"];

/// Whole-word phrases that make a refusal about the account, its
/// credentials, or another kind of limit: never a connection limit, even
/// with `482` or a connection-limit phrase present.
const NOT_CONNECTION_LIMIT_PHRASES: &[&[&str]] = &[
    &["login", "attempts"],
    &["failed", "logins"],
    &["locked"],
    &["expired"],
    &["quota"],
    &["download", "limit"],
    &["rate", "limit"],
    &["out", "of", "sequence"],
];

/// Whether a refused greeting, `AUTHINFO` or `MODE READER` reply says the
/// account is at its connection limit.
///
/// NNTP has no code for "connection limit": backends use `482`, or say so
/// only in the text. Matching is on whole words, case-insensitively, so
/// "rate" never matches inside "separate" nor "locked" inside "blocked":
///
/// - excluded when the text names login attempts, failed logins, a locked or
///   expired account, a quota, a download or rate limit, or commands out of
///   sequence (`NOT_CONNECTION_LIMIT_PHRASES`);
/// - otherwise a connection limit for code `482`, or for "too many
///   \[simultaneous|concurrent|open|active\] connections", "max\[imum\]
///   \[number of\] \[simultaneous|concurrent\] connections", "connection limit",
///   "connections limit", "session limit", "connections exceeded",
///   "connection count exceeded" or "limit of N connections" (with
///   "connection", "conns" or "sessions" accepted for "connections").
///
/// Callers check `481` first: [`classify_authinfo_reply`] treats it as bad
/// credentials whatever its text says.
#[must_use]
pub fn is_connection_limit_reply(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    if NOT_CONNECTION_LIMIT_PHRASES
        .iter()
        .any(|phrase| contains_phrase(&words, phrase))
    {
        return false;
    }
    crate::protocol::StatusCode::parse(reply.as_bytes()).is_some_and(|code| code.as_u16() == 482)
        || names_connection_limit(&words)
}

fn contains_phrase(words: &[&str], phrase: &[&str]) -> bool {
    (0..words.len()).any(|start| words.get(start..start + phrase.len()) == Some(phrase))
}

fn names_connection_limit(words: &[&str]) -> bool {
    let at = |i: usize| words.get(i).copied();
    let is = |i: usize, set: &[&str]| at(i).is_some_and(|word| set.contains(&word));
    (0..words.len()).any(|i| {
        let too_many = at(i) == Some("too") && at(i + 1) == Some("many") && {
            let noun =
                i + 2 + usize::from(is(i + 2, &["simultaneous", "concurrent", "open", "active"]));
            is(noun, CONNECTION_NOUNS)
        };
        let maximum = is(i, &["max", "maximum"]) && {
            let mut noun = i + 1;
            if at(noun) == Some("number") && at(noun + 1) == Some("of") {
                noun += 2;
            }
            noun += usize::from(is(noun, &["simultaneous", "concurrent"]));
            is(noun, CONNECTION_NOUNS)
        };
        let named_limit =
            is(i, &["connection", "connections", "session"]) && at(i + 1) == Some("limit");
        let exceeded = (at(i) == Some("connections") && at(i + 1) == Some("exceeded"))
            || (at(i) == Some("connection")
                && at(i + 1) == Some("count")
                && at(i + 2) == Some("exceeded"));
        let limit_of_n = at(i) == Some("limit")
            && at(i + 1) == Some("of")
            && at(i + 2).is_some_and(|word| word.bytes().all(|b| b.is_ascii_digit()))
            && is(i + 3, CONNECTION_NOUNS);
        too_many || maximum || named_limit || exceeded || limit_of_n
    })
}

impl ConnectionError {
    /// Classify a refusal from a backend that answered, or `None` for any
    /// other failure (network, TLS, configuration).
    #[must_use]
    pub fn login_refusal(&self) -> Option<LoginRefusal> {
        match self {
            Self::ConnectionLimitExceeded { .. } => Some(LoginRefusal::ConnectionLimit),
            Self::AuthenticationFailed { response, .. }
            | Self::UnexpectedAuthResponse { response, .. } => {
                Some(match classify_authinfo_reply(response) {
                    AuthinfoReply::BadCredentials => LoginRefusal::Credentials,
                    AuthinfoReply::ConnectionLimit => LoginRefusal::ConnectionLimit,
                    AuthinfoReply::Other => LoginRefusal::Rejected,
                })
            }
            Self::InvalidGreeting { greeting, .. } => {
                Some(if is_connection_limit_reply(greeting) {
                    LoginRefusal::ConnectionLimit
                } else {
                    LoginRefusal::ServiceUnavailable
                })
            }
            Self::ReaderModeRefused { response, .. } => {
                Some(if is_connection_limit_reply(response) {
                    LoginRefusal::ConnectionLimit
                } else {
                    LoginRefusal::ReaderModeRefused
                })
            }
            _ => None,
        }
    }

    /// The backend answered but would not let this session in.
    ///
    /// Every kind of refusal backs off: unlike a network error, an immediate
    /// retry gets the same answer, so the pool backs off instead of logging
    /// in again for every request.
    #[must_use]
    pub fn is_login_refusal(&self) -> bool {
        self.login_refusal().is_some()
    }

    /// A refusal that says the account is at its connection limit: the only
    /// refusal that may lower an adaptive connection cap.
    #[must_use]
    pub fn is_connection_limit_refusal(&self) -> bool {
        self.login_refusal() == Some(LoginRefusal::ConnectionLimit)
    }

    pub fn dns_resolver_init(
        backend: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::DnsResolverInit {
            backend: backend.into(),
            source: Box::new(source),
        }
    }

    pub fn dns_resolver_build(
        backend: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::DnsResolverBuild {
            backend: backend.into(),
            source: Box::new(source),
        }
    }

    pub fn dns_lookup(
        backend: impl Into<String>,
        address: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::DnsLookup {
            backend: backend.into(),
            address: address.into(),
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn connection_limit_replies_are_told_apart_from_other_refusals() {
        let cases: &[(&str, bool)] = &[
            ("482 Too many connections", true),
            ("482 Authentication rejected", true),
            ("482 Too many connections from separate hosts", true),
            (
                "482 connection limit reached, further connections blocked",
                true,
            ),
            ("502 Too many simultaneous connections", true),
            ("502 Too Many Connections", true),
            ("502 Too many open conns", true),
            ("502 max connection reached", true),
            ("502 max conns (8)", true),
            ("502 Too many connections for this account", true),
            ("502 connection limit (10) reached", true),
            ("502 connection limit (100) reached", true),
            ("400 Connections limit exceeded", true),
            ("400 Session limit reached", true),
            ("400 too many connections from your address", true),
            ("502 Maximum number of connections reached", true),
            ("502 maximum concurrent sessions exceeded", true),
            ("502 Connection count exceeded", true),
            ("502 connections exceeded", true),
            ("502 You have reached your limit of 20 connections", true),
            ("400 Too many sessions", true),
            ("482 commands issued out of sequence", false),
            ("482 Authentication commands issued out of sequence", false),
            ("502 Too many failed login attempts, account locked", false),
            ("482 Too many failed logins", false),
            ("481 Download limit exceeded", false),
            ("502 Too many connections, download limit exceeded", false),
            ("502 Rate limit exceeded", false),
            ("502 Account quota used up", false),
            ("481 Account expired", false),
            ("481 Authentication failed", false),
            ("481 Invalid username or password", false),
            ("502 Limit reached", false),
            ("502 Maximum reached", false),
            ("502 Too many requests", false),
            ("502 Permission denied", false),
            ("400 Service temporarily unavailable", false),
        ];
        for &(reply, expected) in cases {
            assert_eq!(is_connection_limit_reply(reply), expected, "{reply:?}");
        }
    }

    #[test]
    fn authinfo_481_is_bad_credentials_whatever_the_text_says() {
        use AuthinfoReply::{BadCredentials, ConnectionLimit, Other};
        let cases = [
            ("481 Authentication failed", BadCredentials),
            ("481 Download limit exceeded", BadCredentials),
            ("481 Max connections reached", BadCredentials),
            ("481 too many connections", BadCredentials),
            ("482 Too many connections", ConnectionLimit),
            ("502 connection limit (10) reached", ConnectionLimit),
            ("502 Too many failed login attempts, account locked", Other),
            ("502 Permission denied", Other),
        ];
        for (reply, expected) in cases {
            assert_eq!(classify_authinfo_reply(reply), expected, "{reply:?}");
        }
    }

    #[test]
    fn refusal_rule_table() {
        fn auth(response: &str) -> ConnectionError {
            ConnectionError::AuthenticationFailed {
                backend: "backend-a".into(),
                response: response.into(),
            }
        }
        fn user(response: &str) -> ConnectionError {
            ConnectionError::UnexpectedAuthResponse {
                backend: "backend-a".into(),
                response: response.into(),
            }
        }
        fn greeting(text: &str) -> ConnectionError {
            ConnectionError::InvalidGreeting {
                backend: "backend-a".into(),
                greeting: text.into(),
            }
        }
        fn reader(response: &str) -> ConnectionError {
            ConnectionError::ReaderModeRefused {
                backend: "backend-a".into(),
                response: response.into(),
            }
        }
        use LoginRefusal::{
            ConnectionLimit, Credentials, ReaderModeRefused, Rejected, ServiceUnavailable,
        };
        let cases = [
            (auth("481 authentication rejected"), Credentials),
            (auth("481 too many connections"), Credentials),
            (auth("482 anything"), ConnectionLimit),
            (auth("502 Too Many Connections"), ConnectionLimit),
            (auth("502 connection limit (10) reached"), ConnectionLimit),
            (
                auth("502 Too many failed login attempts, account locked"),
                Rejected,
            ),
            (auth("502 permission denied"), Rejected),
            (auth("502 Account quota used up"), Rejected),
            (user("481 unknown user"), Credentials),
            (user("482 too many connections"), ConnectionLimit),
            (user("500 what?"), Rejected),
            (
                greeting("400 too many connections from your address"),
                ConnectionLimit,
            ),
            (
                greeting("400 service temporarily unavailable"),
                ServiceUnavailable,
            ),
            (reader("502 reading not permitted"), ReaderModeRefused),
            (reader("502 Too many connections"), ConnectionLimit),
        ];
        for (error, expected) in cases {
            assert_eq!(error.login_refusal(), Some(expected), "{error}");
            assert!(error.is_login_refusal(), "{error}");
            assert_eq!(
                error.is_connection_limit_refusal(),
                expected == ConnectionLimit,
                "{error}"
            );
        }
    }

    #[test]
    fn login_refusals_are_classified() {
        let backend = || "b".to_string();
        let refusals = [
            ConnectionError::AuthenticationFailed {
                backend: backend(),
                response: "502 connection limit (10) reached".into(),
            },
            ConnectionError::ConnectionLimitExceeded {
                backend: backend(),
                response: "482".into(),
            },
            ConnectionError::InvalidGreeting {
                backend: backend(),
                greeting: "502 too many connections".into(),
            },
            ConnectionError::UnexpectedAuthResponse {
                backend: backend(),
                response: "500".into(),
            },
            ConnectionError::ReaderModeRefused {
                backend: backend(),
                response: "502 reading not permitted".into(),
            },
        ];
        assert!(refusals.iter().all(ConnectionError::is_login_refusal));

        let not_refusals = [
            ConnectionError::IoError(std::io::Error::other("reset")),
            ConnectionError::LoginBackoff {
                backend: backend(),
                retry_in: std::time::Duration::from_secs(5),
            },
            ConnectionError::PoolExhausted {
                backend: backend(),
                max_size: 1,
            },
            ConnectionError::PasswordRequired { backend: backend() },
        ];
        assert!(!not_refusals.iter().any(ConnectionError::is_login_refusal));
        assert!(
            !not_refusals
                .iter()
                .any(ConnectionError::is_connection_limit_refusal)
        );
    }

    #[test]
    fn test_authentication_failed_error() {
        let err = ConnectionError::AuthenticationFailed {
            backend: "news.example.com".to_string(),
            response: "502 Authentication failed".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("news.example.com"));
        assert!(msg.contains("502"));
    }

    #[test]
    fn test_pool_exhausted_error() {
        let err = ConnectionError::PoolExhausted {
            backend: "backend1".to_string(),
            max_size: 20,
        };

        let msg = err.to_string();
        assert!(msg.contains("backend1"));
        assert!(msg.contains("20"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        let conn_err: ConnectionError = io_err.into();

        assert!(matches!(conn_err, ConnectionError::IoError(_)));
    }

    #[test]
    fn test_invalid_greeting_error() {
        let err = ConnectionError::InvalidGreeting {
            backend: "news.server.com".to_string(),
            greeting: "500 Server error".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Invalid greeting"));
        assert!(msg.contains("news.server.com"));
    }

    #[test]
    fn io_error_disconnect_kinds_are_classified_correctly() {
        let broken_pipe = ConnectionError::IoError(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ));
        assert!(
            matches!(&broken_pipe, ConnectionError::IoError(e) if is_disconnect_kind(e.kind()))
        );

        let reset = ConnectionError::IoError(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(matches!(&reset, ConnectionError::IoError(e) if is_disconnect_kind(e.kind())));

        let other = ConnectionError::IoError(std::io::Error::other("other"));
        assert!(!matches!(&other, ConnectionError::IoError(e) if is_disconnect_kind(e.kind())));
    }

    #[test]
    fn test_tls_handshake_error() {
        let err = ConnectionError::TlsHandshake {
            backend: "secure.server.com".to_string(),
            source: Box::new(std::io::Error::other("TLS handshake failed")),
        };

        let msg = err.to_string();
        assert!(msg.contains("TLS handshake failed"));
        assert!(msg.contains("secure.server.com"));
        assert!(err.source().is_some());
    }

    #[test]
    fn test_connection_limit_exceeded_error() {
        let err = ConnectionError::ConnectionLimitExceeded {
            backend: "news.example.com".to_string(),
            response: "482 Connection limit exceeded".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Connection limit exceeded"));
        assert!(msg.contains("news.example.com"));
        assert!(msg.contains("482"));
    }

    #[test]
    fn test_dns_resolver_init_error() {
        let err =
            ConnectionError::dns_resolver_init("dns-backend", std::io::Error::other("bad config"));

        let msg = err.to_string();
        assert!(msg.contains("Failed to initialize DNS resolver"));
        assert!(msg.contains("dns-backend"));
        assert!(err.source().is_some());
    }

    #[test]
    fn test_dns_resolver_build_error() {
        let err =
            ConnectionError::dns_resolver_build("dns-backend", std::io::Error::other("build"));

        let msg = err.to_string();
        assert!(msg.contains("Failed to build DNS resolver"));
        assert!(msg.contains("dns-backend"));
        assert!(err.source().is_some());
    }

    #[test]
    fn test_dns_lookup_error() {
        let err = ConnectionError::dns_lookup(
            "dns-backend",
            "example.com:119",
            std::io::Error::other("lookup"),
        );

        let msg = err.to_string();
        assert!(msg.contains("Failed to resolve example.com:119"));
        assert!(msg.contains("dns-backend"));
        assert!(err.source().is_some());
    }
}
