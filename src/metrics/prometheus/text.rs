//! Prometheus text exposition format 0.0.4 writer.
//!
//! Small enough to own instead of pulling in a client library: the proxy
//! already keeps every value in its own atomics, so all that is needed is a
//! correct serialiser for one scrape.

use std::fmt::Write as _;

/// Prometheus metric type, written on the `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// A sample value. Integers stay exact; floats are used only for values that
/// are fractional by nature, such as seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleValue {
    Int(u64),
    Float(f64),
}

impl From<u64> for SampleValue {
    fn from(value: u64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for SampleValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

/// Builds one exposition document.
///
/// Callers must emit every sample of a family right after its header; the
/// format requires a family's samples to be contiguous.
#[derive(Debug, Default)]
pub struct Exposition {
    out: String,
}

impl Exposition {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
        }
    }

    /// Write the `# HELP` and `# TYPE` lines that open a family.
    pub fn family(&mut self, name: &str, kind: MetricKind, help: &str) {
        self.out.push_str("# HELP ");
        self.out.push_str(name);
        self.out.push(' ');
        push_escaped_help(&mut self.out, help);
        self.out.push_str("\n# TYPE ");
        self.out.push_str(name);
        self.out.push(' ');
        self.out.push_str(kind.as_str());
        self.out.push('\n');
    }

    /// Write one sample line.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl Into<SampleValue>) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (key, value)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                self.out.push_str(key);
                self.out.push_str("=\"");
                push_escaped_label_value(&mut self.out, value);
                self.out.push('"');
            }
            self.out.push('}');
        }
        self.out.push(' ');
        match value.into() {
            SampleValue::Int(v) => {
                let _ = write!(self.out, "{v}");
            }
            SampleValue::Float(v) => push_float(&mut self.out, v),
        }
        self.out.push('\n');
    }

    #[must_use]
    pub fn finish(self) -> String {
        self.out
    }
}

/// Label values escape backslash, double quote, and newline; anything else
/// is passed through, so non-ASCII backend names survive intact.
fn push_escaped_label_value(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// HELP text escapes only backslash and newline.
fn push_escaped_help(out: &mut String, help: &str) {
    for c in help.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn push_float(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str("NaN");
    } else if value.is_infinite() {
        out.push_str(if value > 0.0 { "+Inf" } else { "-Inf" });
    } else {
        let _ = write!(out, "{value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_writes_help_then_type() {
        let mut exposition = Exposition::default();
        exposition.family(
            "nntp_proxy_x_total",
            MetricKind::Counter,
            "A \\ line\nbreak",
        );
        assert_eq!(
            exposition.finish(),
            "# HELP nntp_proxy_x_total A \\\\ line\\nbreak\n# TYPE nntp_proxy_x_total counter\n"
        );
    }

    #[test]
    fn label_values_escape_quote_backslash_and_newline() {
        let mut exposition = Exposition::default();
        exposition.sample("m", &[("backend", "a\"b\\c\nd")], 1u64);
        assert_eq!(exposition.finish(), "m{backend=\"a\\\"b\\\\c\\nd\"} 1\n");
    }

    #[test]
    fn samples_without_labels_omit_braces() {
        let mut exposition = Exposition::default();
        exposition.sample("m", &[], 7u64);
        exposition.sample("f", &[("a", "1"), ("b", "2")], 0.25);
        assert_eq!(exposition.finish(), "m 7\nf{a=\"1\",b=\"2\"} 0.25\n");
    }

    #[test]
    fn special_floats_use_prometheus_spelling() {
        let mut exposition = Exposition::default();
        exposition.sample("a", &[], f64::INFINITY);
        exposition.sample("b", &[], f64::NEG_INFINITY);
        exposition.sample("c", &[], f64::NAN);
        assert_eq!(exposition.finish(), "a +Inf\nb -Inf\nc NaN\n");
    }
}
