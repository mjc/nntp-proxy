//! In-memory log capture for TUI display

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Maximum number of log lines to keep in memory
const MAX_LOG_LINES: usize = 1000;

/// Shared log buffer accessible by both tracing subscriber and TUI
#[derive(Clone)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    /// Create a new log buffer
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES))),
        }
    }

    /// Add a log line to the buffer
    pub fn push(&self, line: String) {
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() >= MAX_LOG_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }

    /// Get recent log lines (returns a copy to avoid holding lock)
    #[must_use]
    pub fn recent_lines(&self, count: usize) -> Vec<String> {
        if let Ok(lines) = self.lines.lock() {
            lines.iter().rev().take(count).rev().cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Get all log lines (returns a copy to avoid holding lock)
    #[must_use]
    pub fn all_lines(&self) -> Vec<String> {
        if let Ok(lines) = self.lines.lock() {
            lines.iter().cloned().collect()
        } else {
            Vec::new()
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Writer that appends to LogBuffer
pub struct LogWriter {
    buffer: LogBuffer,
    line_buffer: String,
}

impl LogWriter {
    /// Create a new log writer
    #[must_use]
    pub fn new(buffer: LogBuffer) -> Self {
        Self {
            buffer,
            line_buffer: String::with_capacity(256),
        }
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = std::str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        
        for c in s.chars() {
            if c == '\n' {
                // Complete line - push to buffer
                if !self.line_buffer.is_empty() {
                    self.buffer.push(self.line_buffer.clone());
                    self.line_buffer.clear();
                }
            } else {
                self.line_buffer.push(c);
            }
        }
        
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush any remaining content
        if !self.line_buffer.is_empty() {
            self.buffer.push(self.line_buffer.clone());
            self.line_buffer.clear();
        }
        Ok(())
    }
}

/// MakeWriter implementation for tracing_subscriber
pub struct LogMakeWriter {
    buffer: LogBuffer,
}

impl LogMakeWriter {
    /// Create a new MakeWriter for the given buffer
    #[must_use]
    pub fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogMakeWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter::new(self.buffer.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_log_buffer_push_and_retrieve() {
        let buffer = LogBuffer::new();
        buffer.push("Line 1".to_string());
        buffer.push("Line 2".to_string());
        buffer.push("Line 3".to_string());

        let lines = buffer.all_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Line 1");
        assert_eq!(lines[2], "Line 3");
    }

    #[test]
    fn test_log_buffer_capacity_limit() {
        let buffer = LogBuffer::new();
        
        // Add more than MAX_LOG_LINES
        for i in 0..1500 {
            buffer.push(format!("Line {}", i));
        }

        let lines = buffer.all_lines();
        assert_eq!(lines.len(), MAX_LOG_LINES);
        // Oldest lines should be dropped
        assert_eq!(lines[0], "Line 500");
    }

    #[test]
    fn test_log_buffer_recent_lines() {
        let buffer = LogBuffer::new();
        for i in 0..10 {
            buffer.push(format!("Line {}", i));
        }

        let recent = buffer.recent_lines(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0], "Line 7");
        assert_eq!(recent[1], "Line 8");
        assert_eq!(recent[2], "Line 9");
    }

    #[test]
    fn test_log_writer_splits_lines() {
        let buffer = LogBuffer::new();
        let mut writer = LogWriter::new(buffer.clone());

        writer.write_all(b"Line 1\nLine 2\nLine 3\n").unwrap();
        writer.flush().unwrap();

        let lines = buffer.all_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Line 1");
        assert_eq!(lines[1], "Line 2");
        assert_eq!(lines[2], "Line 3");
    }

    #[test]
    fn test_log_writer_partial_lines() {
        let buffer = LogBuffer::new();
        let mut writer = LogWriter::new(buffer.clone());

        writer.write_all(b"Partial ").unwrap();
        writer.write_all(b"line\n").unwrap();
        writer.flush().unwrap();

        let lines = buffer.all_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "Partial line");
    }

    #[test]
    fn test_log_writer_flush_incomplete() {
        let buffer = LogBuffer::new();
        let mut writer = LogWriter::new(buffer.clone());

        writer.write_all(b"No newline").unwrap();
        writer.flush().unwrap();

        let lines = buffer.all_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "No newline");
    }
}
