//! Protocol-related type-safe wrappers for NNTP primitives

use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, Cow};
use std::fmt;

use super::ValidationError;

/// A validated NNTP message ID
///
/// Message IDs in NNTP must be enclosed in angle brackets per RFC 3977 Section 3.6.
/// This type ensures message IDs are always properly formatted.
///
/// # Format
/// - Must start with '<' and end with '>'
/// - Cannot be empty (must contain at least 3 characters: `<x>`)
/// - Example: `<12345@example.com>`
///
/// # Performance
/// Uses `Cow<'a, str>` to support both zero-copy parsing (borrowed) and owned storage.
/// - Parser creates `MessageId<'a>` with `Cow::Borrowed` for zero-copy performance
/// - Cache/storage converts to `MessageId<'static>` with `Cow::Owned` for persistence
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId<'a>(Cow<'a, str>);

impl<'a> MessageId<'a> {
    /// Create a new owned MessageId from a String, validating the format
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
    /// assert_eq!(msgid.as_str(), "<12345@example.com>");
    ///
    /// // Invalid: missing angle brackets
    /// assert!(MessageId::new("12345@example.com".to_string()).is_err());
    /// ```
    pub fn new(s: String) -> Result<Self, ValidationError> {
        if s.len() < 3 {
            return Err(ValidationError::InvalidMessageId(
                "Message ID too short (minimum 3 characters)".to_string(),
            ));
        }
        if !s.starts_with('<') || !s.ends_with('>') {
            return Err(ValidationError::InvalidMessageId(
                "Message ID must be enclosed in angle brackets".to_string(),
            ));
        }
        Ok(Self(Cow::Owned(s)))
    }

    /// Create a borrowed MessageId from a string slice, validating the format (zero-copy).
    ///
    /// This is the fastest way to create a MessageId when you have a borrowed string.
    /// No allocation occurs - the MessageId borrows from the input.
    ///
    /// # Performance
    /// **Zero-copy**: This method does NOT allocate. The returned MessageId borrows from `s`.
    /// Use this in parsers for maximum performance.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::from_str("<12345@example.com>").unwrap();
    /// assert_eq!(msgid.as_str(), "<12345@example.com>");
    /// ```
    #[inline]
    pub fn from_str(s: &'a str) -> Result<Self, ValidationError> {
        if s.len() < 3 {
            return Err(ValidationError::InvalidMessageId(
                "Message ID too short (minimum 3 characters)".to_string(),
            ));
        }
        if !s.starts_with('<') || !s.ends_with('>') {
            return Err(ValidationError::InvalidMessageId(
                "Message ID must be enclosed in angle brackets".to_string(),
            ));
        }
        Ok(Self(Cow::Borrowed(s)))
    }

    /// Create a borrowed MessageId from a pre-validated string slice (zero-copy, unchecked).
    ///
    /// # Safety
    /// The caller MUST ensure that:
    /// - `s.len() >= 3`
    /// - `s.starts_with('<')`
    /// - `s.ends_with('>')`
    ///
    /// This is used by the parser after it has already validated these conditions
    /// to avoid redundant checks.
    ///
    /// # Performance
    /// **Zero-copy**: This is the absolute fastest way to create a MessageId.
    /// No allocation, no validation overhead.
    #[inline(always)]
    pub unsafe fn from_str_unchecked(s: &'a str) -> Self {
        Self(Cow::Borrowed(s))
    }

    /// Create an owned MessageId from a string, automatically adding angle brackets if needed
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::from_str_or_wrap("12345@example.com").unwrap();
    /// assert_eq!(msgid.as_str(), "<12345@example.com>");
    ///
    /// let msgid2 = MessageId::from_str_or_wrap("<12345@example.com>").unwrap();
    /// assert_eq!(msgid2.as_str(), "<12345@example.com>");
    /// ```
    pub fn from_str_or_wrap(s: impl AsRef<str>) -> Result<MessageId<'static>, ValidationError> {
        let s = s.as_ref();
        if s.is_empty() {
            return Err(ValidationError::InvalidMessageId(
                "Message ID cannot be empty".to_string(),
            ));
        }

        let wrapped = if s.starts_with('<') && s.ends_with('>') {
            s.to_string()
        } else {
            format!("<{}>", s)
        };

        MessageId::new(wrapped)
    }

    /// Get the message ID as a string slice
    #[must_use]
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the message ID without angle brackets
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
    /// assert_eq!(msgid.without_brackets(), "12345@example.com");
    /// ```
    #[must_use]
    #[inline]
    pub fn without_brackets(&self) -> &str {
        let s: &str = &self.0;
        &s[1..s.len() - 1]
    }

    /// Extract a message ID from an NNTP command line (returns owned MessageId)
    ///
    /// Looks for a message ID (text enclosed in angle brackets) in the command.
    /// Ensures '<' comes before '>' to form a proper pair.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::extract_from_command("ARTICLE <12345@example.com>").unwrap();
    /// assert_eq!(msgid.as_str(), "<12345@example.com>");
    /// ```
    pub fn extract_from_command(command: &str) -> Option<MessageId<'static>> {
        let start = command.find('<')?;
        // Search for '>' only after the '<' position (end is relative to slice start)
        let end = command[start..].find('>')?;
        // Include the '>' character: start + end gives position of '>', +1 for exclusive end
        MessageId::new(command[start..=start + end].to_string()).ok()
    }

    /// Converts this `MessageId` into an owned `MessageId<'static>`, consuming `self`.
    ///
    /// This method is useful when you need to store a `MessageId` beyond the lifetime of the input string.
    ///
    /// Consumes `self` and always returns an owned `MessageId<'static>`. If the underlying data is already owned,
    /// this will not allocate, but will still call `into_owned()` on the inner `Cow`.
    pub fn into_owned(self) -> MessageId<'static> {
        MessageId(Cow::Owned(self.0.into_owned()))
    }

    /// Creates an owned `MessageId<'static>` by cloning the data if necessary.
    ///
    /// This method borrows `self` and returns an owned `MessageId<'static>`. If the underlying data is already owned,
    /// this is a cheap clone. Otherwise, it allocates and copies the data.
    pub fn to_owned(&self) -> MessageId<'static> {
        MessageId(Cow::Owned(self.0.clone().into_owned()))
    }
}

impl<'a> AsRef<str> for MessageId<'a> {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'a> Borrow<str> for MessageId<'a> {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl<'a> fmt::Display for MessageId<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for MessageId<'static> {
    type Error = ValidationError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        MessageId::new(s)
    }
}

impl<'a> From<MessageId<'a>> for String {
    fn from(msgid: MessageId<'a>) -> Self {
        msgid.0.into_owned()
    }
}

impl<'a> Serialize for MessageId<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MessageId<'static> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        MessageId::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_message_id() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
        assert_eq!(msgid.without_brackets(), "12345@example.com");
    }

    #[test]
    fn test_complex_message_id() {
        let msgid =
            MessageId::new("<very-long-id.123.abc@subdomain.example.org>".to_string()).unwrap();
        assert_eq!(
            msgid.without_brackets(),
            "very-long-id.123.abc@subdomain.example.org"
        );
    }

    #[test]
    fn test_message_id_without_brackets_rejected() {
        let result = MessageId::new("12345@example.com".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_message_id_missing_start_bracket() {
        let result = MessageId::new("12345@example.com>".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_message_id_missing_end_bracket() {
        let result = MessageId::new("<12345@example.com".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_message_id() {
        let result = MessageId::new("".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_message_id_too_short() {
        let result = MessageId::new("<>".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_minimal_valid_message_id() {
        let msgid = MessageId::new("<x>".to_string()).unwrap();
        assert_eq!(msgid.without_brackets(), "x");
    }

    #[test]
    fn test_from_str_or_wrap_with_brackets() {
        let msgid = MessageId::from_str_or_wrap("<12345@example.com>").unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
    }

    #[test]
    fn test_from_str_or_wrap_without_brackets() {
        let msgid = MessageId::from_str_or_wrap("12345@example.com").unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
    }

    #[test]
    fn test_from_str_or_wrap_empty() {
        let result = MessageId::from_str_or_wrap("");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_from_command() {
        let msgid = MessageId::extract_from_command("ARTICLE <12345@example.com>").unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
    }

    #[test]
    fn test_extract_from_command_body() {
        let msgid = MessageId::extract_from_command("BODY <test@news.server.com>").unwrap();
        assert_eq!(msgid.as_str(), "<test@news.server.com>");
    }

    #[test]
    fn test_extract_from_command_with_extra_text() {
        let msgid =
            MessageId::extract_from_command("ARTICLE <12345@example.com> extra text").unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
    }

    #[test]
    fn test_extract_from_command_no_message_id() {
        let result = MessageId::extract_from_command("LIST");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_from_command_malformed() {
        let result = MessageId::extract_from_command("ARTICLE >12345@example.com<");
        assert!(result.is_none());
    }

    #[test]
    fn test_display() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        assert_eq!(format!("{}", msgid), "<12345@example.com>");
    }

    #[test]
    fn test_as_ref() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let s: &str = msgid.as_ref();
        assert_eq!(s, "<12345@example.com>");
    }

    #[test]
    fn test_try_from_string() {
        let msgid: MessageId = "<12345@example.com>".to_string().try_into().unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
    }

    #[test]
    fn test_into_string() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let s: String = msgid.into();
        assert_eq!(s, "<12345@example.com>");
    }

    #[test]
    fn test_clone() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let cloned = msgid.clone();
        assert_eq!(msgid, cloned);
    }

    #[test]
    fn test_equality() {
        let msgid1 = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let msgid2 = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let msgid3 = MessageId::new("<54321@example.com>".to_string()).unwrap();
        assert_eq!(msgid1, msgid2);
        assert_ne!(msgid1, msgid3);
    }

    #[test]
    fn test_serde_roundtrip() {
        let msgid = MessageId::new("<12345@example.com>".to_string()).unwrap();
        let json = serde_json::to_string(&msgid).unwrap();
        assert_eq!(json, "\"<12345@example.com>\"");

        let deserialized: MessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msgid);
    }

    #[test]
    fn test_serde_invalid() {
        let json = "\"invalid-msgid\"";
        let result: Result<MessageId, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
