//! Protocol-related type-safe wrappers for NNTP primitives

use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, Cow};
use std::fmt;
use std::str::FromStr;

use super::ValidationError;

/// A validated NNTP message ID (RFC 3977 §3.6)
///
/// Message IDs must be enclosed in angle brackets.
/// Uses `Cow<'a, str>` for zero-copy parsing (borrowed) and owned storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId<'a>(Cow<'a, str>);

impl<'a> MessageId<'a> {
    /// Create owned MessageId from String with validation
    pub fn new(s: String) -> Result<Self, ValidationError> {
        Self::validate(&s)?;
        Ok(Self(Cow::Owned(s)))
    }

    /// Create borrowed MessageId from &str (zero-copy)
    #[inline]
    pub fn from_borrowed(s: &'a str) -> Result<Self, ValidationError> {
        Self::validate(s)?;
        Ok(Self(Cow::Borrowed(s)))
    }

    /// Create from pre-validated string (zero-copy, unchecked)
    ///
    /// # Safety
    /// Caller must ensure: `s.len() >= 3`, `s.starts_with('<')`, `s.ends_with('>')`
    #[inline(always)]
    pub unsafe fn from_str_unchecked(s: &'a str) -> Self {
        Self(Cow::Borrowed(s))
    }

    /// Create owned MessageId, auto-wrapping in angle brackets if needed
    pub fn from_str_or_wrap(s: impl AsRef<str>) -> Result<MessageId<'static>, ValidationError> {
        let s = s.as_ref();
        if s.is_empty() {
            return Err(ValidationError::InvalidMessageId("empty".to_string()));
        }
        let wrapped = if s.starts_with('<') && s.ends_with('>') {
            s.to_string()
        } else {
            format!("<{}>", s)
        };
        MessageId::new(wrapped)
    }

    /// Extract message ID from NNTP command (returns owned)
    pub fn extract_from_command(command: &str) -> Option<MessageId<'static>> {
        let start = command.find('<')?;
        let end = command[start..].find('>')?;
        MessageId::new(command[start..=start + end].to_string()).ok()
    }

    #[inline]
    fn validate(s: &str) -> Result<(), ValidationError> {
        if s.len() < 3 || !s.starts_with('<') || !s.ends_with('>') {
            Err(ValidationError::InvalidMessageId(
                "must be <...>".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    #[must_use]
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    #[inline]
    pub fn without_brackets(&self) -> &str {
        &self.0[1..self.0.len() - 1]
    }

    pub fn into_owned(self) -> MessageId<'static> {
        MessageId(Cow::Owned(self.0.into_owned()))
    }

    pub fn to_owned(&self) -> MessageId<'static> {
        MessageId(Cow::Owned(self.0.clone().into_owned()))
    }
}

impl FromStr for MessageId<'static> {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MessageId::new(s.to_string())
    }
}

impl<'a> AsRef<str> for MessageId<'a> {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'a> std::ops::Deref for MessageId<'a> {
    type Target = str;
    #[inline]
    fn deref(&self) -> &Self::Target {
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
        f.write_str(&self.0)
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
    fn test_message_id_validation() {
        assert!(MessageId::new("<12345@example.com>".to_string()).is_ok());
        assert!(MessageId::new("missing-brackets".to_string()).is_err());
        assert!(MessageId::new("<>".to_string()).is_err());
        assert!(MessageId::new("".to_string()).is_err());
    }

    #[test]
    fn test_message_id_without_brackets() {
        let msgid = MessageId::new("<test@example.com>".to_string()).unwrap();
        assert_eq!(msgid.without_brackets(), "test@example.com");
    }

    #[test]
    fn test_extract_from_command() {
        let msgid = MessageId::extract_from_command("ARTICLE <12345@example.com>").unwrap();
        assert_eq!(msgid.as_str(), "<12345@example.com>");
        assert!(MessageId::extract_from_command("LIST").is_none());
    }

    #[test]
    fn test_from_str_or_wrap() {
        assert_eq!(
            MessageId::from_str_or_wrap("<test@example.com>")
                .unwrap()
                .as_str(),
            "<test@example.com>"
        );
        assert_eq!(
            MessageId::from_str_or_wrap("test@example.com")
                .unwrap()
                .as_str(),
            "<test@example.com>"
        );
        // Empty string should error
        assert!(MessageId::from_str_or_wrap("").is_err());
    }

    #[test]
    fn test_from_borrowed() {
        let s = "<borrowed@example.com>";
        let msgid = MessageId::from_borrowed(s).unwrap();
        assert_eq!(msgid.as_str(), s);

        // Invalid borrowed should error
        assert!(MessageId::from_borrowed("no-brackets").is_err());
        assert!(MessageId::from_borrowed("<>").is_err());
    }

    #[test]
    fn test_as_str() {
        let msgid = MessageId::new("<test@example.com>".to_string()).unwrap();
        assert_eq!(msgid.as_str(), "<test@example.com>");
    }

    #[test]
    fn test_into_owned() {
        let s = "<borrowed@example.com>";
        let msgid = MessageId::from_borrowed(s).unwrap();
        let owned = msgid.into_owned();
        assert_eq!(owned.as_str(), s);
    }

    #[test]
    fn test_to_owned() {
        let s = "<borrowed@example.com>";
        let msgid = MessageId::from_borrowed(s).unwrap();
        let owned = msgid.to_owned();
        assert_eq!(owned.as_str(), s);
        // Original still valid
        assert_eq!(msgid.as_str(), s);
    }

    #[test]
    fn test_from_str() {
        let msgid: MessageId = "<test@example.com>".parse().unwrap();
        assert_eq!(msgid.as_str(), "<test@example.com>");

        assert!("invalid".parse::<MessageId>().is_err());
    }

    #[test]
    fn test_try_from_string() {
        let msgid = MessageId::try_from("<test@example.com>".to_string()).unwrap();
        assert_eq!(msgid.as_str(), "<test@example.com>");

        assert!(MessageId::try_from("invalid".to_string()).is_err());
    }

    #[test]
    fn test_into_string() {
        let msgid = MessageId::new("<test@example.com>".to_string()).unwrap();
        let s: String = msgid.into();
        assert_eq!(s, "<test@example.com>");
    }

    #[test]
    fn test_extract_from_command_edge_cases() {
        // Multiple message IDs - should extract first
        assert_eq!(
            MessageId::extract_from_command("ARTICLE <first@test> <second@test>")
                .unwrap()
                .as_str(),
            "<first@test>"
        );

        // No closing bracket
        assert!(MessageId::extract_from_command("ARTICLE <incomplete").is_none());

        // Empty command
        assert!(MessageId::extract_from_command("").is_none());
    }

    #[test]
    fn test_validation_edge_cases() {
        // Too short (len < 3)
        assert!(MessageId::new("<>".to_string()).is_err());
        assert!(MessageId::new("<a".to_string()).is_err());
        assert!(MessageId::new("a>".to_string()).is_err());

        // Missing brackets
        assert!(MessageId::new("<no-end".to_string()).is_err());
        assert!(MessageId::new("no-start>".to_string()).is_err());

        // Valid minimal
        assert!(MessageId::new("<a>".to_string()).is_ok());
    }
}
