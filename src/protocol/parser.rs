//! Type-safe NNTP command parser using nom
//!
//! This module provides a complete, type-safe parser for NNTP commands according to RFC 3977.
//! Uses nom for zero-copy, compile-time validated parsing.
//!
//! # RFC 3977 Compliance
//!
//! This parser implements the NNTP protocol as specified in:
//! - **RFC 3977**: Network News Transfer Protocol (NNTP)
//!   <https://datatracker.ietf.org/doc/html/rfc3977>
//! - **RFC 4643**: Network News Transfer Protocol (NNTP) Extension for Authentication
//!   <https://datatracker.ietf.org/doc/html/rfc4643>
//!
//! ## Command Syntax
//!
//! Per [RFC 3977 Section 3.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.1):
//! - Commands consist of a keyword, optional arguments separated by spaces/tabs
//! - Commands are terminated by CRLF
//! - Command lines MUST NOT exceed 512 octets (including CRLF)
//! - Arguments MUST NOT exceed 497 octets
//! - Keywords are case-insensitive
//!
//! See [RFC 3977 Section 9.2](https://datatracker.ietf.org/doc/html/rfc3977#section-9.2) for formal ABNF syntax.

use nom::{
    branch::alt,
    bytes::complete::{tag_no_case, take_until, take_while1},
    character::complete::{char, digit1, space0, space1},
    combinator::{map, map_res, opt},
    sequence::{delimited, preceded, terminated},
    IResult, Parser,
};

use crate::types::MessageId;

/// Article specifier - by message-ID or by number
///
/// # RFC 3977 References
///
/// Article retrieval commands ([Section 6.2](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2)) accept different forms:
///
/// - **message-id**: [RFC 3977 Section 6.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1) (first form)
///   - Syntax: `<message-id>` where message-id is as defined in [Section 3.6](https://datatracker.ietf.org/doc/html/rfc3977#section-3.6)
///   - Example: `<45223423@example.com>`
///
/// - **article number**: [RFC 3977 Section 6.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1) (second form)
///   - Syntax: 1-16 digit number between 1 and 2,147,483,647
///   - Example: `3000234`
///
/// - **current article**: [RFC 3977 Section 6.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1) (third form)
///   - No argument uses currently selected article in current group
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArticleSpec {
    /// Article by message-ID (e.g., <123@example.com>)
    ///
    /// [RFC 3977 Section 3.6](https://datatracker.ietf.org/doc/html/rfc3977#section-3.6):
    /// Message-ID must be 3-250 octets, begin with '<', end with '>',
    /// contain only printable US-ASCII characters
    ByMessageId(MessageId),
    
    /// Article by number in current group (stateful)
    ///
    /// [RFC 3977 Section 6](https://datatracker.ietf.org/doc/html/rfc3977#section-6):
    /// Article numbers MUST be between 1 and 2,147,483,647.
    /// Requires a currently selected newsgroup.
    ByNumber(u32),
    
    /// Current article (no argument provided)
    ///
    /// [RFC 3977 Section 6.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1):
    /// If no argument, uses current article number in currently selected newsgroup
    Current,
}

/// Authentication command variants
///
/// # RFC 4643 - NNTP Authentication
///
/// [RFC 4643](https://datatracker.ietf.org/doc/html/rfc4643) defines the AUTHINFO extension.
///
/// ## AUTHINFO USER
///
/// [RFC 4643 Section 2.3](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3):
/// - Syntax: `AUTHINFO USER username`
/// - Sent by client to begin authentication
/// - Username is case-sensitive
///
/// ## AUTHINFO PASS
///
/// [RFC 4643 Section 2.4](https://datatracker.ietf.org/doc/html/rfc4643#section-2.4):
/// - Syntax: `AUTHINFO PASS password`
/// - Sent after AUTHINFO USER
/// - Password is case-sensitive
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthInfo<'a> {
    /// AUTHINFO USER <username>
    User(&'a str),
    /// AUTHINFO PASS <password>
    Pass(&'a str),
}

/// Range specification for OVER/XOVER and HDR/XHDR commands
///
/// # RFC 3977 References
///
/// [RFC 3977 Section 8.3](https://datatracker.ietf.org/doc/html/rfc3977#section-8.3) - OVER command
///
/// The range argument may be any of the following:
/// - An article number (e.g., `3000234`)
/// - An article number followed by a dash to indicate all following (e.g., `3000234-`)
/// - An article number followed by a dash followed by another article number (e.g., `3000234-3000240`)
///
/// If the second number is less than the first number, the range contains no articles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArticleRange {
    /// Single article number
    Single(u32),
    /// Range from start to end (inclusive)
    Range { start: u32, end: u32 },
    /// Range from start to current article
    RangeFrom(u32),
    /// Current article only
    Current,
}

/// Group selection mode for LISTGROUP
///
/// # RFC 3977 References
///
/// [RFC 3977 Section 6.1.2](https://datatracker.ietf.org/doc/html/rfc3977#section-6.1.2) - LISTGROUP command
///
/// Syntax: `LISTGROUP [group [range]]`
///
/// - If no group is specified, the currently selected newsgroup is used
/// - If an optional range is specified, only articles within the range are included
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListGroupSpec<'a> {
    /// List current group
    Current,
    /// List specific group
    Group(&'a str),
    /// List specific group with range
    GroupWithRange { group: &'a str, range: ArticleRange },
}

/// Parsed NNTP command with full type safety
///
/// # RFC 3977 Command Reference
///
/// This enum represents all standard NNTP commands as defined in RFC 3977,
/// plus common extensions (XOVER, XHDR, AUTHINFO, MODE STREAM).
///
/// Commands are organized by functional category per the RFC structure.
#[derive(Debug, Clone, PartialEq)]
pub enum Command<'a> {
    // ========================================================================
    // Article Retrieval Commands - RFC 3977 Section 6.2
    // ========================================================================
    
    /// ARTICLE [message-id|number]
    ///
    /// [RFC 3977 Section 6.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1)
    ///
    /// Returns the full article (headers and body).
    ///
    /// **Syntax:**
    /// - `ARTICLE <message-id>` - Retrieve by message-ID
    /// - `ARTICLE <number>` - Retrieve by article number in current group
    /// - `ARTICLE` - Retrieve current article
    ///
    /// **Responses:**
    /// - 220 n message-id (multi-line)
    /// - 412 No newsgroup selected
    /// - 420 Current article number is invalid
    /// - 423 No article with that number
    /// - 430 No article with that message-id
    Article(ArticleSpec),
    
    /// BODY [message-id|number]
    ///
    /// [RFC 3977 Section 6.2.3](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.3)
    ///
    /// Returns only the article body (without headers).
    ///
    /// **Syntax:** Same as ARTICLE
    ///
    /// **Responses:**
    /// - 222 n message-id (multi-line)
    /// - 412/420/423/430 (same as ARTICLE)
    Body(ArticleSpec),
    
    /// HEAD [message-id|number]
    ///
    /// [RFC 3977 Section 6.2.2](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.2)
    ///
    /// Returns only the article headers (without body).
    ///
    /// **Syntax:** Same as ARTICLE
    ///
    /// **Responses:**
    /// - 221 n message-id (multi-line)
    /// - 412/420/423/430 (same as ARTICLE)
    Head(ArticleSpec),
    
    /// STAT [message-id|number]
    ///
    /// [RFC 3977 Section 6.2.4](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.4)
    ///
    /// Checks if article exists without retrieving it.
    ///
    /// **Syntax:** Same as ARTICLE
    ///
    /// **Responses:**
    /// - 223 n message-id
    /// - 412/420/423/430 (same as ARTICLE)
    Stat(ArticleSpec),

    // ========================================================================
    // Group and Article Navigation - RFC 3977 Section 6.1
    // ========================================================================
    
    /// GROUP <group-name>
    ///
    /// [RFC 3977 Section 6.1.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.1.1)
    ///
    /// Selects a newsgroup and returns summary information.
    ///
    /// **Syntax:** `GROUP newsgroup-name`
    ///
    /// **Responses:**
    /// - 211 number low high group
    /// - 411 No such newsgroup
    Group(&'a str),
    
    /// LISTGROUP [group [range]]
    ///
    /// [RFC 3977 Section 6.1.2](https://datatracker.ietf.org/doc/html/rfc3977#section-6.1.2)
    ///
    /// Selects a newsgroup and returns article numbers.
    ///
    /// **Syntax:**
    /// - `LISTGROUP` - List articles in current group
    /// - `LISTGROUP newsgroup` - Select and list articles
    /// - `LISTGROUP newsgroup range` - Select and list articles in range
    ///
    /// **Responses:**
    /// - 211 number low high group (multi-line)
    /// - 411 No such newsgroup
    /// - 412 No newsgroup selected
    ListGroup(ListGroupSpec<'a>),
    
    /// NEXT
    ///
    /// [RFC 3977 Section 6.1.4](https://datatracker.ietf.org/doc/html/rfc3977#section-6.1.4)
    ///
    /// Advances to the next article in the current group.
    ///
    /// **Syntax:** `NEXT`
    ///
    /// **Responses:**
    /// - 223 n message-id
    /// - 412 No newsgroup selected
    /// - 420 Current article number is invalid
    /// - 421 No next article in this group
    Next,
    
    /// LAST
    ///
    /// [RFC 3977 Section 6.1.3](https://datatracker.ietf.org/doc/html/rfc3977#section-6.1.3)
    ///
    /// Moves to the previous article in the current group.
    ///
    /// **Syntax:** `LAST`
    ///
    /// **Responses:**
    /// - 223 n message-id
    /// - 412 No newsgroup selected
    /// - 420 Current article number is invalid
    /// - 422 No previous article in this group
    Last,

    // ========================================================================
    // Information Commands - RFC 3977 Section 7 and Section 8
    // ========================================================================
    
    /// LIST [keyword [wildmat|argument]]
    ///
    /// [RFC 3977 Section 7.6.1](https://datatracker.ietf.org/doc/html/rfc3977#section-7.6.1)
    ///
    /// Returns server information. Keywords include:
    /// - `ACTIVE` - Active newsgroups ([Section 7.6.3](https://datatracker.ietf.org/doc/html/rfc3977#section-7.6.3))
    /// - `NEWSGROUPS` - Group descriptions ([Section 7.6.6](https://datatracker.ietf.org/doc/html/rfc3977#section-7.6.6))
    /// - `OVERVIEW.FMT` - Overview database format ([Section 8.4](https://datatracker.ietf.org/doc/html/rfc3977#section-8.4))
    /// - `HEADERS` - Available headers for HDR ([Section 8.6](https://datatracker.ietf.org/doc/html/rfc3977#section-8.6))
    ///
    /// **Syntax:**
    /// - `LIST` - Defaults to LIST ACTIVE
    /// - `LIST keyword [argument]`
    ///
    /// **Responses:**
    /// - 215 Information follows (multi-line)
    /// - 501 Syntax error
    List(Option<&'a str>),
    
    /// OVER [message-id|range]
    ///
    /// [RFC 3977 Section 8.3](https://datatracker.ietf.org/doc/html/rfc3977#section-8.3)
    ///
    /// Returns overview database information for articles.
    ///
    /// **Syntax:**
    /// - `OVER` - Overview for current article
    /// - `OVER range` - Overview for article range
    /// - `OVER <message-id>` - Overview for specific article (if MSGID capability)
    ///
    /// **Responses:**
    /// - 224 Overview information follows (multi-line)
    /// - 412 No newsgroup selected
    /// - 420 Current article number is invalid
    /// - 423 No articles in that range
    /// - 430 No article with that message-id
    Over(Option<ArticleRange>),
    
    /// XOVER [range]
    ///
    /// Legacy extension command, precursor to OVER.
    /// Functionality identical to OVER but without message-id support.
    XOver(Option<ArticleRange>),
    
    /// HDR <header> [message-id|range]
    ///
    /// [RFC 3977 Section 8.5](https://datatracker.ietf.org/doc/html/rfc3977#section-8.5)
    ///
    /// Returns specific header fields from articles.
    ///
    /// **Syntax:**
    /// - `HDR field` - Header from current article
    /// - `HDR field range` - Header from article range
    /// - `HDR field <message-id>` - Header from specific article
    ///
    /// **Responses:**
    /// - 225 Headers follow (multi-line)
    /// - 412 No newsgroup selected
    /// - 420 Current article number is invalid
    /// - 423 No articles in that range
    /// - 430 No article with that message-id
    Hdr { header: &'a str, range: Option<ArticleRange> },
    
    /// XHDR <header> [message-id|range]
    ///
    /// Legacy extension command, precursor to HDR.
    /// Functionality identical to HDR.
    XHdr { header: &'a str, range: Option<ArticleRange> },

    // ========================================================================
    // Posting Commands - RFC 3977 Section 6.3
    // ========================================================================
    
    /// POST
    ///
    /// [RFC 3977 Section 6.3.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.3.1)
    ///
    /// Posts a new article from a news-reading client.
    ///
    /// **Syntax:** `POST`
    ///
    /// **Responses:**
    /// - 340 Send article to be posted
    /// - 440 Posting not permitted
    /// - 240 Article received OK (after article sent)
    /// - 441 Posting failed (after article sent)
    Post,
    
    /// IHAVE <message-id>
    ///
    /// [RFC 3977 Section 6.3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-6.3.2)
    ///
    /// Transfers an already-posted article between servers.
    ///
    /// **Syntax:** `IHAVE <message-id>`
    ///
    /// **Responses:**
    /// - 335 Send article to be transferred
    /// - 435 Article not wanted
    /// - 436 Transfer not possible; try again later
    /// - 235 Article transferred OK (after article sent)
    /// - 437 Transfer rejected; do not retry (after article sent)
    Ihave(MessageId),

    // ========================================================================
    // Authentication - RFC 4643
    // ========================================================================
    
    /// AUTHINFO USER/PASS
    ///
    /// [RFC 4643](https://datatracker.ietf.org/doc/html/rfc4643)
    ///
    /// Authenticates the client to the server.
    AuthInfo(AuthInfo<'a>),

    // ========================================================================
    // Administrative Commands - RFC 3977 Section 5 and Section 7
    // ========================================================================
    
    /// CAPABILITIES
    ///
    /// [RFC 3977 Section 5.2](https://datatracker.ietf.org/doc/html/rfc3977#section-5.2)
    ///
    /// Returns server capabilities.
    ///
    /// **Syntax:** `CAPABILITIES [keyword]`
    ///
    /// **Responses:**
    /// - 101 Capability list follows (multi-line)
    Capabilities,
    
    /// MODE READER
    ///
    /// [RFC 3977 Section 5.3](https://datatracker.ietf.org/doc/html/rfc3977#section-5.3)
    ///
    /// Instructs a mode-switching server to switch to reader mode.
    ///
    /// **Syntax:** `MODE READER`
    ///
    /// **Responses:**
    /// - 200 Posting allowed
    /// - 201 Posting prohibited
    /// - 502 Reading service permanently unavailable
    ModeReader,
    
    /// MODE STREAM
    ///
    /// [RFC 4644](https://datatracker.ietf.org/doc/html/rfc4644)
    ///
    /// Switches to streaming mode for high-volume article transfer.
    ModeStream,
    
    /// DATE
    ///
    /// [RFC 3977 Section 7.1](https://datatracker.ietf.org/doc/html/rfc3977#section-7.1)
    ///
    /// Returns the server's current date and time in UTC.
    ///
    /// **Syntax:** `DATE`
    ///
    /// **Responses:**
    /// - 111 yyyymmddhhmmss
    Date,
    
    /// HELP
    ///
    /// [RFC 3977 Section 7.2](https://datatracker.ietf.org/doc/html/rfc3977#section-7.2)
    ///
    /// Returns a short summary of available commands.
    ///
    /// **Syntax:** `HELP`
    ///
    /// **Responses:**
    /// - 100 Help text follows (multi-line)
    Help,
    
    /// NEWGROUPS <date> <time> [GMT]
    ///
    /// [RFC 3977 Section 7.3](https://datatracker.ietf.org/doc/html/rfc3977#section-7.3)
    ///
    /// Returns list of newsgroups created since specified date/time.
    ///
    /// **Syntax:** `NEWGROUPS yymmdd|yyyymmdd hhmmss [GMT]`
    ///
    /// **Responses:**
    /// - 231 List of new newsgroups follows (multi-line)
    NewGroups { date: &'a str, time: &'a str, gmt: bool },
    
    /// NEWNEWS <wildmat> <date> <time> [GMT]
    ///
    /// [RFC 3977 Section 7.4](https://datatracker.ietf.org/doc/html/rfc3977#section-7.4)
    ///
    /// Returns message-IDs of articles posted since specified date/time.
    ///
    /// **Syntax:** `NEWNEWS wildmat yymmdd|yyyymmdd hhmmss [GMT]`
    ///
    /// **Responses:**
    /// - 230 List of new articles follows (multi-line)
    NewNews {
        wildmat: &'a str,
        date: &'a str,
        time: &'a str,
        gmt: bool,
    },
    
    /// QUIT
    ///
    /// [RFC 3977 Section 5.4](https://datatracker.ietf.org/doc/html/rfc3977#section-5.4)
    ///
    /// Closes the connection.
    ///
    /// **Syntax:** `QUIT`
    ///
    /// **Responses:**
    /// - 205 Connection closing
    Quit,

    // ========================================================================
    // Extension/Unknown Commands
    // ========================================================================
    
    /// Unknown command (will be forwarded to backend)
    ///
    /// Commands not recognized by the parser are preserved as-is for
    /// forwarding to the backend server. This allows support for proprietary
    /// extensions without modifying the parser.
    Unknown { command: &'a str, args: Option<&'a str> },
}

impl<'a> Command<'a> {
    /// Check if this command requires stateful session (needs GROUP context)
    pub fn is_stateful(&self) -> bool {
        matches!(
            self,
            Command::Article(ArticleSpec::ByNumber(_) | ArticleSpec::Current)
                | Command::Body(ArticleSpec::ByNumber(_) | ArticleSpec::Current)
                | Command::Head(ArticleSpec::ByNumber(_) | ArticleSpec::Current)
                | Command::Stat(ArticleSpec::ByNumber(_) | ArticleSpec::Current)
                | Command::Group(_)
                | Command::ListGroup(_)
                | Command::Next
                | Command::Last
                | Command::Over(_)
                | Command::XOver(_)
                | Command::Hdr { .. }
                | Command::XHdr { .. }
        )
    }

    /// Check if this is an authentication command
    pub fn is_auth(&self) -> bool {
        matches!(self, Command::AuthInfo(_))
    }

    /// Check if this is a high-throughput command (article by message-ID)
    pub fn is_high_throughput(&self) -> bool {
        matches!(
            self,
            Command::Article(ArticleSpec::ByMessageId(_))
                | Command::Body(ArticleSpec::ByMessageId(_))
                | Command::Head(ArticleSpec::ByMessageId(_))
                | Command::Stat(ArticleSpec::ByMessageId(_))
        )
    }
}

// ============================================================================
// Parser Helper Functions
// ============================================================================

/// Parse a message-ID
///
/// # RFC 3977 Section 3.6
///
/// [Message-IDs](https://datatracker.ietf.org/doc/html/rfc3977#section-3.6)
///
/// ## Requirements:
///
/// - MUST begin with `<` and end with `>`
/// - MUST be between 3 and 250 octets in length
/// - MUST NOT contain octets other than printable US-ASCII characters
/// - The `>` MUST NOT appear except at the end
///
/// ## ABNF Syntax (Section 9.8):
///
/// ```text
/// message-id = "<" 1*248A-NOTGT ">"
/// A-NOTGT    = %x21-3D / %x3F-7E  ; exclude ">"
/// ```
///
/// # Examples
///
/// - `<45223423@example.com>`
/// - `<668929@example.org>`
/// - `<i.am.a.test.article@example.com>`
fn parse_message_id(input: &str) -> IResult<&str, MessageId> {
    let (input, msgid_str) = delimited(
        char('<'),
        take_until(">"),
        char('>'),
    ).parse(input)?;
    
    // Construct the full message-ID with brackets
    let full_msgid = format!("<{}>", msgid_str);
    match MessageId::new(full_msgid) {
        Ok(msgid) => Ok((input, msgid)),
        Err(_) => Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Verify,
        ))),
    }
}

/// Parse an article number
///
/// # RFC 3977 Section 6
///
/// [Article numbering](https://datatracker.ietf.org/doc/html/rfc3977#section-6)
///
/// ## Requirements:
///
/// - Article numbers MUST lie between 1 and 2,147,483,647, inclusive
/// - Client and server MAY use leading zeroes but MUST NOT use more than 16 digits
/// - Article numbers MUST be issued in order of arrival timestamp
///
/// ## ABNF Syntax (Section 9.8):
///
/// ```text
/// article-number = 1*16DIGIT
/// DIGIT = %x30-39
/// ```
///
/// # Examples
///
/// - `1`
/// - `3000234`
/// - `0003000234` (with leading zeros)
fn parse_article_number(input: &str) -> IResult<&str, u32> {
    map_res(digit1, |s: &str| s.parse::<u32>()).parse(input)
}

/// Parse article specification (message-ID, number, or current)
///
/// # RFC 3977 Section 6.2.1
///
/// [ARTICLE command syntax](https://datatracker.ietf.org/doc/html/rfc3977#section-6.2.1)
///
/// The ARTICLE command (and BODY, HEAD, STAT) accept three forms:
///
/// ## First form (message-id specified):
/// ```text
/// ARTICLE message-id
/// ```
///
/// ## Second form (article number specified):
/// ```text
/// ARTICLE number
/// ```
///
/// ## Third form (current article number used):
/// ```text
/// ARTICLE
/// ```
///
/// This helper parses the argument portion (message-id or number).
/// The third form (no argument) is handled by `parse_optional_article_spec`.
fn parse_article_spec(input: &str) -> IResult<&str, ArticleSpec> {
    alt((
        map(parse_message_id, ArticleSpec::ByMessageId),
        map(parse_article_number, ArticleSpec::ByNumber),
    )).parse(input)
}

/// Parse optional article specification
///
/// Returns `ArticleSpec::Current` if no argument is provided.
///
/// This handles the third form of article retrieval commands where
/// the current article in the currently selected newsgroup is used.
///
/// # RFC 3977 Section 6.2.1
///
/// > In the third form, the article indicated by the current article
/// > number in the currently selected newsgroup is used.
fn parse_optional_article_spec(input: &str) -> IResult<&str, ArticleSpec> {
    let (input, _) = space0(input)?;
    if input.is_empty() || input.starts_with('\r') || input.starts_with('\n') {
        return Ok((input, ArticleSpec::Current));
    }
    parse_article_spec(input)
}

/// Parse article range for OVER/XOVER and HDR/XHDR commands
///
/// # RFC 3977 Section 8.3 - OVER Command
///
/// [Range argument](https://datatracker.ietf.org/doc/html/rfc3977#section-8.3)
///
/// The range argument may be any of the following:
///
/// ## Single article number:
/// ```text
/// OVER 3000234
/// ```
///
/// ## Article number followed by a dash (all following):
/// ```text
/// OVER 3000234-
/// ```
///
/// ## Article number followed by dash and another number:
/// ```text
/// OVER 3000234-3000240
/// ```
///
/// ## Notes:
///
/// - If the second number is less than the first number, the range contains no articles
/// - Omitting the range is equivalent to the range `1-` being specified
///
/// # ABNF Syntax (Section 9.8):
///
/// ```text
/// range = article-number ["-" [article-number]]
/// ```
#[allow(dead_code)] // Will be used for XOVER/OVER commands
fn parse_article_range(input: &str) -> IResult<&str, ArticleRange> {
    alt((
        // Range: start-end
        map(
            (parse_article_number, char('-'), parse_article_number),
            |(start, _, end)| ArticleRange::Range { start, end },
        ),
        // Range from: start-
        map(
            terminated(parse_article_number, char('-')),
            ArticleRange::RangeFrom,
        ),
        // Single number
        map(parse_article_number, ArticleRange::Single),
    )).parse(input)
}

/// Parse AUTHINFO subcommand
///
/// # RFC 4643 - NNTP Authentication
///
/// [AUTHINFO Extension](https://datatracker.ietf.org/doc/html/rfc4643)
///
/// ## AUTHINFO USER
///
/// [Section 2.3](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3):
/// ```text
/// AUTHINFO USER username
/// ```
///
/// ## AUTHINFO PASS
///
/// [Section 2.4](https://datatracker.ietf.org/doc/html/rfc4643#section-2.4):
/// ```text
/// AUTHINFO PASS password
/// ```
///
/// ## Notes:
///
/// - Both username and password are case-sensitive
/// - AUTHINFO PASS typically follows AUTHINFO USER
/// - Authentication state persists for the session
fn parse_authinfo(input: &str) -> IResult<&str, AuthInfo<'_>> {
    preceded(
        space1,
        alt((
            map(
                preceded(tag_no_case("USER"), preceded(space1, take_while1(|c| c != '\r' && c != '\n'))),
                AuthInfo::User,
            ),
            map(
                preceded(tag_no_case("PASS"), preceded(space1, take_while1(|c| c != '\r' && c != '\n'))),
                AuthInfo::Pass,
            ),
        )),
    ).parse(input)
}

/// Parse a token (wildmat pattern, keyword, or argument)
///
/// # RFC 3977 Section 9.8
///
/// [General non-terminals](https://datatracker.ietf.org/doc/html/rfc3977#section-9.8)
///
/// ```text
/// token = 1*P-CHAR
/// P-CHAR = A-CHAR / UTF8-non-ascii
/// A-CHAR = %x21-7E
/// ```
///
/// Tokens are strings of printable characters without whitespace.
/// Used for newsgroup names, keywords, arguments, etc.
fn parse_token(input: &str) -> IResult<&str, &str> {
    take_while1(|c: char| !c.is_ascii_whitespace() && c != '\r' && c != '\n')(input)
}

/// Parse an NNTP command line
///
/// # RFC 3977 Section 3.1 - Commands and Responses
///
/// [Command syntax](https://datatracker.ietf.org/doc/html/rfc3977#section-3.1)
///
/// ## Command Format:
///
/// Commands in NNTP MUST consist of a keyword, which MAY be followed by one
/// or more arguments. Keywords and arguments MUST each be separated by one
/// or more space or TAB characters.
///
/// ## Requirements:
///
/// - Command lines MUST NOT exceed 512 octets (including CRLF)
/// - Arguments MUST NOT exceed 497 octets
/// - Keywords are case-insensitive
/// - Commands are terminated by CRLF
///
/// # RFC 3977 Section 9.2 - Command ABNF
///
/// [Formal syntax](https://datatracker.ietf.org/doc/html/rfc3977#section-9.2):
///
/// ```text
/// command-line = command EOL
/// command = X-command
/// X-command = keyword *(WS token)
/// keyword = ALPHA 2*(ALPHA / DIGIT / "." / "-")
/// ```
///
/// # Returns
///
/// - `Ok((remaining, Command))` - Successfully parsed command
/// - `Err(nom::Err<E>)` - Parse error
///
/// # Examples
///
/// ```rust,ignore
/// use nntp_proxy::protocol::parser::parse_command;
///
/// let (_, cmd) = parse_command("ARTICLE <123@example.com>").unwrap();
/// let (_, cmd) = parse_command("GROUP misc.test").unwrap();
/// let (_, cmd) = parse_command("QUIT").unwrap();
/// ```
pub fn parse_command(input: &str) -> IResult<&str, Command<'_>> {
    let input = input.trim();
    
    alt((
        // Article retrieval commands
        map(
            preceded(tag_no_case("ARTICLE"), parse_optional_article_spec),
            Command::Article,
        ),
        map(
            preceded(tag_no_case("BODY"), parse_optional_article_spec),
            Command::Body,
        ),
        map(
            preceded(tag_no_case("HEAD"), parse_optional_article_spec),
            Command::Head,
        ),
        map(
            preceded(tag_no_case("STAT"), parse_optional_article_spec),
            Command::Stat,
        ),
        
        // Group commands
        map(
            preceded((tag_no_case("GROUP"), space1), parse_token),
            Command::Group,
        ),
        // LISTGROUP - with optional group and range
        map(
            preceded(
                tag_no_case("LISTGROUP"),
                opt(preceded(space1, parse_token)),
            ),
            |group| match group {
                None => Command::ListGroup(ListGroupSpec::Current),
                Some(g) => Command::ListGroup(ListGroupSpec::Group(g)),
            },
        ),
        
        // IHAVE <message-id>
        map(
            preceded((tag_no_case("IHAVE"), space1), parse_message_id),
            Command::Ihave,
        ),
        
        // NEWGROUPS <date> <time> [GMT]
        map(
            preceded(
                (tag_no_case("NEWGROUPS"), space1),
                (
                    parse_token,
                    preceded(space1, parse_token),
                    opt(preceded(space1, tag_no_case("GMT"))),
                ),
            ),
            |(date, time, gmt)| Command::NewGroups {
                date,
                time,
                gmt: gmt.is_some(),
            },
        ),
        
        // NEWNEWS <wildmat> <date> <time> [GMT]
        map(
            preceded(
                (tag_no_case("NEWNEWS"), space1),
                (
                    parse_token,
                    preceded(space1, parse_token),
                    preceded(space1, parse_token),
                    opt(preceded(space1, tag_no_case("GMT"))),
                ),
            ),
            |(wildmat, date, time, gmt)| Command::NewNews {
                wildmat,
                date,
                time,
                gmt: gmt.is_some(),
            },
        ),
        
        // Authentication
        map(
            preceded(tag_no_case("AUTHINFO"), parse_authinfo),
            Command::AuthInfo,
        ),
        
        // Simple commands (no arguments)
        map(tag_no_case("CAPABILITIES"), |_| Command::Capabilities),
        map(tag_no_case("DATE"), |_| Command::Date),
        map(tag_no_case("HELP"), |_| Command::Help),
        map(tag_no_case("NEXT"), |_| Command::Next),
        map(tag_no_case("LAST"), |_| Command::Last),
        map(tag_no_case("POST"), |_| Command::Post),
        map(tag_no_case("QUIT"), |_| Command::Quit),
        
        // MODE commands
        map(
            preceded((tag_no_case("MODE"), space1), tag_no_case("READER")),
            |_| Command::ModeReader,
        ),
        map(
            preceded((tag_no_case("MODE"), space1), tag_no_case("STREAM")),
            |_| Command::ModeStream,
        ),
        
        // LIST command (with optional keyword)
        map(
            preceded(
                tag_no_case("LIST"),
                opt(preceded(space1, parse_token)),
            ),
            Command::List,
        ),
        
        // Unknown command - catch-all
        map(
            (
                parse_token,
                opt(preceded(space1, take_while1(|c| c != '\r' && c != '\n'))),
            ),
            |(cmd, args)| Command::Unknown { command: cmd, args },
        ),
    )).parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_id() {
        let (rest, msgid) = parse_message_id("<test@example.com>").unwrap();
        assert_eq!(rest, "");
        assert_eq!(msgid.as_str(), "<test@example.com>");
    }

    #[test]
    fn test_parse_article_number() {
        let (rest, num) = parse_article_number("12345").unwrap();
        assert_eq!(rest, "");
        assert_eq!(num, 12345);
    }

    #[test]
    fn test_parse_article_by_msgid() {
        let (_, cmd) = parse_command("ARTICLE <test@example.com>").unwrap();
        match cmd {
            Command::Article(ArticleSpec::ByMessageId(msgid)) => {
                assert_eq!(msgid.as_str(), "<test@example.com>");
            }
            _ => panic!("Expected Article with MessageId"),
        }
    }

    #[test]
    fn test_parse_article_by_number() {
        let (_, cmd) = parse_command("ARTICLE 12345").unwrap();
        match cmd {
            Command::Article(ArticleSpec::ByNumber(num)) => {
                assert_eq!(num, 12345);
            }
            _ => panic!("Expected Article with number"),
        }
    }

    #[test]
    fn test_parse_article_current() {
        let (_, cmd) = parse_command("ARTICLE").unwrap();
        match cmd {
            Command::Article(ArticleSpec::Current) => {}
            _ => panic!("Expected Article with Current"),
        }
    }

    #[test]
    fn test_parse_authinfo_user() {
        let (_, cmd) = parse_command("AUTHINFO USER testuser").unwrap();
        match cmd {
            Command::AuthInfo(AuthInfo::User(user)) => {
                assert_eq!(user, "testuser");
            }
            _ => panic!("Expected AuthInfo User"),
        }
    }

    #[test]
    fn test_parse_authinfo_pass() {
        let (_, cmd) = parse_command("AUTHINFO PASS secret123").unwrap();
        match cmd {
            Command::AuthInfo(AuthInfo::Pass(pass)) => {
                assert_eq!(pass, "secret123");
            }
            _ => panic!("Expected AuthInfo Pass"),
        }
    }

    #[test]
    fn test_parse_group() {
        let (_, cmd) = parse_command("GROUP alt.test").unwrap();
        match cmd {
            Command::Group(group) => {
                assert_eq!(group, "alt.test");
            }
            _ => panic!("Expected Group"),
        }
    }

    #[test]
    fn test_parse_simple_commands() {
        let commands = vec![
            ("QUIT", Command::Quit),
            ("DATE", Command::Date),
            ("HELP", Command::Help),
            ("NEXT", Command::Next),
            ("LAST", Command::Last),
            ("POST", Command::Post),
            ("CAPABILITIES", Command::Capabilities),
        ];

        for (input, expected) in commands {
            let (_, cmd) = parse_command(input).unwrap();
            assert_eq!(cmd, expected);
        }
    }

    #[test]
    fn test_case_insensitive() {
        let inputs = vec!["QUIT", "quit", "Quit", "qUiT"];
        for input in inputs {
            let (_, cmd) = parse_command(input).unwrap();
            assert_eq!(cmd, Command::Quit);
        }
    }

    #[test]
    fn test_is_stateful() {
        assert!(parse_command("ARTICLE 123").unwrap().1.is_stateful());
        assert!(parse_command("GROUP alt.test").unwrap().1.is_stateful());
        assert!(!parse_command("ARTICLE <test@example.com>").unwrap().1.is_stateful());
        assert!(!parse_command("QUIT").unwrap().1.is_stateful());
    }

    #[test]
    fn test_is_high_throughput() {
        assert!(parse_command("ARTICLE <test@example.com>").unwrap().1.is_high_throughput());
        assert!(parse_command("BODY <test@example.com>").unwrap().1.is_high_throughput());
        assert!(!parse_command("ARTICLE 123").unwrap().1.is_high_throughput());
        assert!(!parse_command("QUIT").unwrap().1.is_high_throughput());
    }

    #[test]
    fn test_unknown_command() {
        let (_, cmd) = parse_command("XFEATURE-COMPRESS GZIP").unwrap();
        match cmd {
            Command::Unknown { command, args } => {
                assert_eq!(command, "XFEATURE-COMPRESS");
                assert_eq!(args, Some("GZIP"));
            }
            _ => panic!("Expected Unknown command"),
        }
    }

    // ========================================================================
    // Additional Comprehensive RFC 3977 Compliance Tests
    // ========================================================================

    #[test]
    fn test_article_all_three_forms() {
        // RFC 3977 Section 6.2.1 - First form (message-id)
        let (_, cmd) = parse_command("ARTICLE <45223423@example.com>").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::ByMessageId(_))));

        // Second form (article number)
        let (_, cmd) = parse_command("ARTICLE 3000234").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::ByNumber(3000234))));

        // Third form (current article)
        let (_, cmd) = parse_command("ARTICLE").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::Current)));
    }

    #[test]
    fn test_body_head_stat_variants() {
        // Test BODY command
        assert!(parse_command("BODY <test@example.com>").is_ok());
        assert!(parse_command("BODY 123").is_ok());
        assert!(parse_command("BODY").is_ok());

        // Test HEAD command
        assert!(parse_command("HEAD <test@example.com>").is_ok());
        assert!(parse_command("HEAD 456").is_ok());
        assert!(parse_command("HEAD").is_ok());

        // Test STAT command
        assert!(parse_command("STAT <test@example.com>").is_ok());
        assert!(parse_command("STAT 789").is_ok());
        assert!(parse_command("STAT").is_ok());
    }

    #[test]
    fn test_group_listgroup() {
        // GROUP command
        let (_, cmd) = parse_command("GROUP misc.test").unwrap();
        assert!(matches!(cmd, Command::Group("misc.test")));

        // LISTGROUP variations
        let (_, cmd) = parse_command("LISTGROUP").unwrap();
        assert!(matches!(cmd, Command::ListGroup(ListGroupSpec::Current)));

        let (_, cmd) = parse_command("LISTGROUP alt.binaries.test").unwrap();
        assert!(matches!(cmd, Command::ListGroup(ListGroupSpec::Group(_))));
    }

    #[test]
    fn test_next_last_navigation() {
        // RFC 3977 Section 6.1.4 - NEXT
        let (_, cmd) = parse_command("NEXT").unwrap();
        assert!(matches!(cmd, Command::Next));

        // RFC 3977 Section 6.1.3 - LAST
        let (_, cmd) = parse_command("LAST").unwrap();
        assert!(matches!(cmd, Command::Last));
    }

    #[test]
    fn test_list_variants() {
        // LIST with no arguments
        let (_, cmd) = parse_command("LIST").unwrap();
        assert!(matches!(cmd, Command::List(None)));

        // LIST ACTIVE
        assert!(parse_command("LIST ACTIVE").is_ok());

        // LIST NEWSGROUPS
        assert!(parse_command("LIST NEWSGROUPS").is_ok());

        // LIST OVERVIEW.FMT
        assert!(parse_command("LIST OVERVIEW.FMT").is_ok());

        // LIST ACTIVE.TIMES
        assert!(parse_command("LIST ACTIVE.TIMES").is_ok());
    }

    #[test]
    fn test_posting_commands() {
        // POST - RFC 3977 Section 6.3.1
        let (_, cmd) = parse_command("POST").unwrap();
        assert!(matches!(cmd, Command::Post));

        // IHAVE - RFC 3977 Section 6.3.2
        let (_, cmd) = parse_command("IHAVE <i.am.an.article@example.com>").unwrap();
        match cmd {
            Command::Ihave(msgid) => {
                assert_eq!(msgid.as_str(), "<i.am.an.article@example.com>");
            }
            _ => panic!("Expected IHAVE command"),
        }
    }

    #[test]
    fn test_administrative_commands() {
        // CAPABILITIES - RFC 3977 Section 5.2
        assert!(parse_command("CAPABILITIES").is_ok());

        // MODE READER - RFC 3977 Section 5.3
        assert!(parse_command("MODE READER").is_ok());

        // MODE STREAM - RFC 4644
        assert!(parse_command("MODE STREAM").is_ok());

        // DATE - RFC 3977 Section 7.1
        assert!(parse_command("DATE").is_ok());

        // HELP - RFC 3977 Section 7.2
        assert!(parse_command("HELP").is_ok());

        // QUIT - RFC 3977 Section 5.4
        assert!(parse_command("QUIT").is_ok());
    }

    #[test]
    fn test_newgroups_newnews() {
        // NEWGROUPS - RFC 3977 Section 7.3
        let (_, cmd) = parse_command("NEWGROUPS 19990624 000000 GMT").unwrap();
        match cmd {
            Command::NewGroups { date, time, gmt } => {
                assert_eq!(date, "19990624");
                assert_eq!(time, "000000");
                assert!(gmt);
            }
            _ => panic!("Expected NewGroups"),
        }

        // Without GMT
        let (_, cmd) = parse_command("NEWGROUPS 20251014 120000").unwrap();
        assert!(matches!(cmd, Command::NewGroups { gmt: false, .. }));

        // NEWNEWS - RFC 3977 Section 7.4
        let (_, cmd) = parse_command("NEWNEWS news.*,sci.* 19990624 000000 GMT").unwrap();
        match cmd {
            Command::NewNews { wildmat, date, time, gmt } => {
                assert_eq!(wildmat, "news.*,sci.*");
                assert_eq!(date, "19990624");
                assert_eq!(time, "000000");
                assert!(gmt);
            }
            _ => panic!("Expected NewNews"),
        }
    }

    #[test]
    fn test_authentication_rfc4643() {
        // AUTHINFO USER
        let (_, cmd) = parse_command("AUTHINFO USER testuser").unwrap();
        match cmd {
            Command::AuthInfo(AuthInfo::User(u)) => assert_eq!(u, "testuser"),
            _ => panic!("Expected AUTHINFO USER"),
        }

        // AUTHINFO PASS
        let (_, cmd) = parse_command("AUTHINFO PASS secret123").unwrap();
        match cmd {
            Command::AuthInfo(AuthInfo::Pass(p)) => assert_eq!(p, "secret123"),
            _ => panic!("Expected AUTHINFO PASS"),
        }

        // Case insensitive command, but case-sensitive arguments
        let (_, cmd) = parse_command("authinfo user TestUser").unwrap();
        match cmd {
            Command::AuthInfo(AuthInfo::User(u)) => assert_eq!(u, "TestUser"),
            _ => panic!("Expected AUTHINFO USER with case-sensitive username"),
        }
    }

    #[test]
    fn test_message_id_validation() {
        // Valid message-IDs from RFC 3977 examples
        assert!(parse_command("ARTICLE <45223423@example.com>").is_ok());
        assert!(parse_command("ARTICLE <668929@example.org>").is_ok());
        assert!(parse_command("ARTICLE <i.am.a.test.article@example.com>").is_ok());

        // Complex message-IDs
        assert!(parse_command("STAT <very.long.message.id.with.dots@subdomain.example.com>").is_ok());
        assert!(parse_command("HEAD <test-id_123@my-server.com>").is_ok());
        assert!(parse_command("BODY <123456789@host.example.org>").is_ok());
    }

    #[test]
    fn test_article_number_limits() {
        // Minimum article number
        let (_, cmd) = parse_command("ARTICLE 1").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::ByNumber(1))));

        // Maximum article number per RFC 3977 Section 6
        let (_, cmd) = parse_command("ARTICLE 2147483647").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::ByNumber(2147483647))));

        // Leading zeros (allowed per RFC 3977)
        let (_, cmd) = parse_command("ARTICLE 0000123").unwrap();
        assert!(matches!(cmd, Command::Article(ArticleSpec::ByNumber(123))));
    }

    #[test]
    fn test_newsgroup_names() {
        // Standard newsgroup names
        assert!(parse_command("GROUP comp.lang.rust").is_ok());
        assert!(parse_command("GROUP alt.binaries.pictures").is_ok());
        assert!(parse_command("GROUP misc.test").is_ok());
        assert!(parse_command("GROUP news.software.nntp").is_ok());

        // Newsgroups with numbers
        assert!(parse_command("GROUP alt.binaries.test.123").is_ok());

        // Newsgroups with hyphens
        assert!(parse_command("GROUP alt.test-group").is_ok());
    }

    #[test]
    fn test_whitespace_tolerance() {
        // Extra leading/trailing spaces
        assert!(parse_command("  QUIT  ").is_ok());
        assert!(parse_command("  GROUP  misc.test  ").is_ok());
        assert!(parse_command("   ARTICLE   12345   ").is_ok());

        // Multiple spaces between command and argument
        assert!(parse_command("GROUP  misc.test").is_ok());
        assert!(parse_command("ARTICLE  123").is_ok());
    }

    #[test]
    fn test_extension_commands() {
        // Unknown/extension commands should be preserved
        let (_, cmd) = parse_command("XFEATURE arg1 arg2").unwrap();
        match cmd {
            Command::Unknown { command, args } => {
                assert_eq!(command, "XFEATURE");
                assert!(args.is_some());
            }
            _ => panic!("Expected Unknown command"),
        }

        // Extension command without arguments
        let (_, cmd) = parse_command("XCUSTOM").unwrap();
        match cmd {
            Command::Unknown { command, args } => {
                assert_eq!(command, "XCUSTOM");
                assert!(args.is_none());
            }
            _ => panic!("Expected Unknown command"),
        }
    }

    #[test]
    fn test_command_helpers() {
        // Test is_stateful for various commands
        assert!(parse_command("GROUP misc.test").unwrap().1.is_stateful());
        assert!(parse_command("ARTICLE 123").unwrap().1.is_stateful());
        assert!(parse_command("NEXT").unwrap().1.is_stateful());
        assert!(parse_command("LAST").unwrap().1.is_stateful());
        assert!(!parse_command("DATE").unwrap().1.is_stateful());
        assert!(!parse_command("QUIT").unwrap().1.is_stateful());

        // Test is_auth
        assert!(parse_command("AUTHINFO USER test").unwrap().1.is_auth());
        assert!(parse_command("AUTHINFO PASS test").unwrap().1.is_auth());
        assert!(!parse_command("QUIT").unwrap().1.is_auth());

        // Test is_high_throughput
        assert!(parse_command("ARTICLE <test@example.com>").unwrap().1.is_high_throughput());
        assert!(!parse_command("ARTICLE 123").unwrap().1.is_high_throughput());
    }
}

