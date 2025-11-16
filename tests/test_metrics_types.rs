use nntp_proxy::metrics::types::*;
use std::num::NonZeroU64;

#[test]
fn test_command_count() {
    let mut count = CommandCount::new(0);
    assert_eq!(count.get(), 0);
    count.increment();
    assert_eq!(count.get(), 1);
}

#[test]
fn test_article_count_average() {
    let count = ArticleCount::new(10);
    assert_eq!(count.average_bytes(1000), Some(100));
    assert_eq!(ArticleCount::new(0).average_bytes(1000), None);
}

#[test]
fn test_microseconds_conversion() {
    let micros = Microseconds::new(1500);
    assert_eq!(micros.as_millis_f64(), 1.5);
}

#[test]
fn test_ttfb_average() {
    let total = TtfbMicros::new(10000);
    let count = NonZeroU64::new(5).unwrap();
    let avg = TtfbMicros::average(total, count);
    assert_eq!(avg.get(), 2.0); // 10000 / 5 / 1000 = 2ms
}

#[test]
fn test_overhead_calculation() {
    let ttfb = Milliseconds::new(10.0);
    let send = Milliseconds::new(0.5);
    let recv = Milliseconds::new(9.0);
    let overhead = OverheadMillis::from_components(ttfb, send, recv);
    assert_eq!(overhead.get(), 0.5);
}

#[test]
fn test_bytes_per_second() {
    let rate = BytesPerSecond::from_delta(1000, 2.0);
    assert_eq!(rate.get(), 500);
}

#[test]
fn test_commands_per_second() {
    let rate = CommandsPerSecond::from_delta(100, 2.0);
    assert_eq!(rate.get(), 50.0);
}

#[test]
fn test_error_rate_percent() {
    let errors = ErrorCount::new(6);
    let commands = CommandCount::new(100);
    let rate = ErrorRatePercent::from_counts(errors, commands);
    assert_eq!(rate.get(), 6.0);
    assert!(rate.is_high());

    let low_rate = ErrorRatePercent::from_counts(ErrorCount::new(2), CommandCount::new(100));
    assert_eq!(low_rate.get(), 2.0);
    assert!(!low_rate.is_high());
}

#[test]
fn test_error_rate_zero_commands() {
    let rate = ErrorRatePercent::from_counts(ErrorCount::new(5), CommandCount::new(0));
    assert_eq!(rate.get(), 0.0);
    assert!(!rate.is_high());
}

#[test]
fn test_command_count_saturating_sub() {
    let a = CommandCount::new(10);
    let b = CommandCount::new(3);
    assert_eq!(a.saturating_sub(b).get(), 7);

    let c = CommandCount::new(5);
    let d = CommandCount::new(10);
    assert_eq!(c.saturating_sub(d).get(), 0); // Saturates at 0
}

#[test]
fn test_error_count_operations() {
    let mut errors = ErrorCount::new(5);
    errors.increment();
    assert_eq!(errors.get(), 6);

    let mut errors2 = ErrorCount::new(3);
    errors2.add(ErrorCount::new(7));
    assert_eq!(errors2.get(), 10);

    assert!(ErrorCount::new(0).is_zero());
    assert!(!ErrorCount::new(1).is_zero());
}

#[test]
fn test_article_count_operations() {
    let mut articles = ArticleCount::new(10);
    articles.increment();
    assert_eq!(articles.get(), 11);
}

#[test]
fn test_failure_count_operations() {
    let mut failures = FailureCount::new(0);
    failures.increment();
    assert_eq!(failures.get(), 1);

    let delta = FailureCount::new(10).saturating_sub(FailureCount::new(3));
    assert_eq!(delta.get(), 7);
}

#[test]
fn test_active_connections_display() {
    let active = ActiveConnections::new(42);
    assert_eq!(format!("{}", active), "42");
    assert_eq!(active.get(), 42);
}

#[test]
fn test_microseconds_to_millis() {
    let micros = Microseconds::new(5000);
    assert_eq!(micros.as_millis_f64(), 5.0);

    let micros2 = Microseconds::new(1500);
    assert_eq!(micros2.as_millis_f64(), 1.5);
}

#[test]
fn test_ttfb_send_recv_averages() {
    let count = NonZeroU64::new(4).unwrap();

    let ttfb = TtfbMicros::average(TtfbMicros::new(8000), count);
    assert_eq!(ttfb.get(), 2.0); // 8000 / 4 / 1000 = 2ms

    let send = SendMicros::average(SendMicros::new(400), count);
    assert_eq!(send.get(), 0.1); // 400 / 4 / 1000 = 0.1ms

    let recv = RecvMicros::average(RecvMicros::new(7200), count);
    assert_eq!(recv.get(), 1.8); // 7200 / 4 / 1000 = 1.8ms
}

#[test]
fn test_overhead_from_averages() {
    let ttfb = Milliseconds::new(5.0);
    let send = Milliseconds::new(0.2);
    let recv = Milliseconds::new(4.5);
    let overhead = OverheadMillis::from_components(ttfb, send, recv);
    assert!((overhead.get() - 0.3).abs() < 1e-10); // Floating point comparison
}

#[test]
fn test_overhead_negative_becomes_zero() {
    // Overhead can be negative in case of measurement error
    let ttfb = Milliseconds::new(5.0);
    let send = Milliseconds::new(3.0);
    let recv = Milliseconds::new(4.0);
    let overhead = OverheadMillis::from_components(ttfb, send, recv);
    assert_eq!(overhead.get(), -2.0); // 5 - 3 - 4 = -2
}

#[test]
fn test_bytes_per_second_rates() {
    let rate1 = BytesPerSecond::from_delta(10000, 2.0);
    assert_eq!(rate1.get(), 5000);

    let rate2 = BytesPerSecond::from_delta(0, 5.0);
    assert_eq!(rate2.get(), 0);

    let rate3 = BytesPerSecond::from_delta(1000, 0.5);
    assert_eq!(rate3.get(), 2000);
}

#[test]
fn test_commands_per_second_rates() {
    let rate1 = CommandsPerSecond::from_delta(100, 2.0);
    assert_eq!(rate1.get(), 50.0);

    let rate2 = CommandsPerSecond::from_delta(0, 5.0);
    assert_eq!(rate2.get(), 0.0);

    let rate3 = CommandsPerSecond::from_delta(50, 0.5);
    assert_eq!(rate3.get(), 100.0);
}

#[test]
fn test_display_implementations() {
    assert_eq!(format!("{}", CommandCount::new(42)), "42");
    assert_eq!(format!("{}", ErrorCount::new(7)), "7");
    assert_eq!(format!("{}", ArticleCount::new(100)), "100");
    assert_eq!(format!("{}", FailureCount::new(3)), "3");
    assert_eq!(format!("{}", ActiveConnections::new(5)), "5");
}
