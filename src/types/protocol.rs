//! Protocol-related type-safe wrappers for NNTP primitives

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(String);

impl MessageId {
    /// Create a new MessageId from a string, validating the format
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
        Ok(Self(s))
    }

    /// Create a MessageId from a string, automatically adding angle brackets if needed
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
    pub fn from_str_or_wrap(s: impl AsRef<str>) -> Result<Self, ValidationError> {
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

        Self::new(wrapped)
    }

    /// Get the message ID as a string slice
    #[must_use]
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
    pub fn without_brackets(&self) -> &str {
        &self.0[1..self.0.len() - 1]
    }

    /// Extract a message ID from an NNTP command line
    ///
    /// Looks for a message ID (text enclosed in angle brackets) in the command.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MessageId;
    ///
    /// let msgid = MessageId::extract_from_command("ARTICLE <12345@example.com>").unwrap();
    /// assert_eq!(msgid.as_str(), "<12345@example.com>");
    /// ```
    pub fn extract_from_command(command: &str) -> Option<Self> {
        let start = command.find('<')?;
        let end = command.find('>')?;
        if end <= start {
            return None;
        }
        Self::new(command[start..=end].to_string()).ok()
    }
}

impl AsRef<str> for MessageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for MessageId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for MessageId {
    type Error = ValidationError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<MessageId> for String {
    fn from(msgid: MessageId) -> Self {
        msgid.0
    }
}

impl Serialize for MessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
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
