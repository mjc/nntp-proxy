//! TUI rendering and layout

use crate::formatting::{format_bytes, format_count};
use crate::tui::app::TuiApp;
use crate::tui::constants::{chart, layout, styles, text};
use crate::tui::helpers::{
    build_chart_data, calculate_chart_bounds, create_sparkline, error_rate_color,
    format_error_rate, format_summary_throughput, format_throughput_label, health_indicator,
    load_percentage_color, magnitude_color,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Chart, Dataset, GraphType, List, ListItem, Paragraph, Wrap},
};

#[allow(clippy::cast_precision_loss)] // Chart labels and percentages are presentation-only values.
const fn counter_as_f64(value: u64) -> f64 {
    // These chart labels and percentages are display-only aggregates; the
    // underlying pipeline counters remain exact integers in the snapshot.
    value as f64
}

#[allow(clippy::cast_precision_loss)] // Pool utilization is displayed as a percentage in the UI.
const fn size_as_f64(value: usize) -> f64 {
    // Pool utilization is presented as a percentage in the UI, not used for control flow.
    value as f64
}

#[allow(clippy::cast_precision_loss)] // Utilization percentages are display-only values.
fn buffer_utilization_percent(in_use: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        size_as_f64(in_use * 100) / size_as_f64(total)
    }
}

const fn backend_error_count_color(errors_4xx: u64, errors_5xx: u64) -> Color {
    if errors_5xx > 0 {
        Color::Red
    } else if errors_4xx > 0 {
        Color::Yellow
    } else {
        styles::VALUE_NEUTRAL
    }
}

const fn connection_failures_color(failures: u64) -> Color {
    if failures > 0 {
        Color::Red
    } else {
        styles::VALUE_NEUTRAL
    }
}

// ============================================================================
// Widget Creation Helpers (Pure Functions)
// ============================================================================

/// Create a bordered block with title and color
#[inline]
fn bordered_block(title: &'static str, border_color: Color) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::new().fg(border_color))
}

// ============================================================================
// Render Functions
// ============================================================================

/// Render the main UI
pub fn render_ui(f: &mut Frame, app: &TuiApp) {
    use crate::tui::app::ViewMode;
    let snapshot = app.snapshot();
    let servers = app.servers();

    // Check if we're in log fullscreen mode
    if app.view_mode() == ViewMode::LogFullscreen {
        // Fullscreen logs - show only title and logs
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(layout::TITLE_HEIGHT),
                Constraint::Min(10), // Most of screen for logs
                Constraint::Length(layout::FOOTER_HEIGHT),
            ])
            .split(f.area());

        render_title(f, chunks[0], snapshot);
        render_logs(f, chunks[1], app);
        render_footer(f, chunks[2]);
        return;
    }

    // Normal mode - show all panels
    // Determine if we have enough space for a log window
    // We need at least 40 lines total to fit everything comfortably
    let show_logs = f.area().height >= layout::MIN_HEIGHT_FOR_LOGS;

    // Create main layout - dynamically add log section based on available space
    let chunks = if show_logs {
        Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(layout::TITLE_HEIGHT),
                Constraint::Length(layout::SUMMARY_HEIGHT),
                Constraint::Min(layout::MIN_CHART_HEIGHT),
                Constraint::Length(layout::LOG_WINDOW_HEIGHT),
                Constraint::Length(layout::FOOTER_HEIGHT),
            ])
            .split(f.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(layout::main_sections())
            .split(f.area())
    };

    // Render each section
    render_title(f, chunks[0], snapshot);
    render_summary(f, chunks[1], app);

    // Backends area now contains 3 columns: backends, chart, and user stats
    render_backends(f, chunks[2], snapshot, servers, app);

    if show_logs {
        render_logs(f, chunks[3], app);
        render_footer(f, chunks[4]);
    } else {
        render_footer(f, chunks[3]);
    }
}

/// Render the title bar
fn render_title(f: &mut Frame, area: Rect, snapshot: &crate::metrics::MetricsSnapshot) {
    let title_line = Line::from(vec![
        "NNTP Proxy ".fg(styles::BORDER_ACTIVE).bold(),
        "- Real-Time Metrics Dashboard".fg(Color::White),
    ]);

    let info_line = Line::from(vec![
        "Uptime: ".fg(styles::LABEL),
        snapshot.format_uptime().fg(styles::VALUE_PRIMARY).bold(),
        "  |  Active: ".fg(styles::LABEL),
        format!(
            "{} connections",
            format_count(snapshot.active_connections as u64)
        )
        .fg(magnitude_color(snapshot.active_connections as u64)),
        "  |  Total: ".fg(styles::LABEL),
        format!("{} connections", format_count(snapshot.total_connections))
            .fg(magnitude_color(snapshot.total_connections)),
    ]);

    let title = Paragraph::new(vec![title_line, info_line])
        .block(bordered_block("", styles::BORDER_ACTIVE))
        .alignment(Alignment::Center);

    f.render_widget(title, area);
}

/// Render summary statistics
fn render_summary(f: &mut Frame, area: Rect, app: &TuiApp) {
    let snapshot = app.snapshot();
    let system_stats = app.system_stats();

    // Get latest throughput from history (extracted for testing)
    let (client_to_backend_str, backend_to_client_str) =
        format_summary_throughput(app.latest_client_throughput());

    // Split summary box into three columns
    let summary_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(area);

    // Left: App summary (uptime, sessions, buffer stats in details mode)
    let left_summary = create_app_summary(
        snapshot,
        system_stats,
        app.buffer_pool(),
        app.show_details(),
    );

    // Middle: Cache summary
    let middle_summary = create_cache_summary(snapshot);

    // Right: Data transfer summary
    let right_summary =
        create_transfer_summary(snapshot, &client_to_backend_str, &backend_to_client_str);

    f.render_widget(left_summary, summary_chunks[0]);
    f.render_widget(middle_summary, summary_chunks[1]);
    f.render_widget(right_summary, summary_chunks[2]);
}

/// Render backend server visualization
fn render_backends(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::Server],
    app: &TuiApp,
) {
    // Split into three columns: backend list, data flow chart, and top users
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(layout::backend_columns())
        .split(area);

    render_backend_list(f, chunks[0], snapshot, servers, app);
    render_data_flow(f, chunks[1], servers, app);
    render_user_stats(f, chunks[2], snapshot);
}

// ============================================================================
// Summary Panel Builders (Pure Functions)
// ============================================================================

/// Create app summary panel (uptime, CPU, memory, buffer stats in details mode)
fn create_app_summary(
    snapshot: &crate::metrics::MetricsSnapshot,
    system_stats: &crate::tui::SystemStats,
    buffer_pool: Option<&crate::pool::BufferPool>,
    show_details: bool,
) -> Paragraph<'static> {
    /// Color for CPU usage based on threshold
    const fn cpu_color(usage: f32) -> Color {
        if usage > 80.0 {
            Color::Red
        } else if usage > 50.0 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    let mut lines = vec![
        Line::from(vec![
            "Uptime: ".fg(styles::LABEL),
            snapshot.format_uptime().fg(styles::VALUE_PRIMARY),
        ]),
        Line::from(vec![
            "Stateful Sessions: ".fg(styles::LABEL),
            format_count(snapshot.stateful_sessions as u64)
                .fg(magnitude_color(snapshot.stateful_sessions as u64)),
        ]),
        Line::from(vec![
            "CPU: ".fg(styles::LABEL),
            format!("{:.1}%", system_stats.cpu_usage).fg(cpu_color(system_stats.cpu_usage)),
        ]),
        Line::from(vec![
            "Memory: ".fg(styles::LABEL),
            format_bytes(system_stats.memory_bytes).fg(styles::VALUE_INFO),
        ]),
    ];

    // Show pipeline stats if any batches have been processed
    if snapshot.pipeline_batches > 0 {
        let avg_batch =
            counter_as_f64(snapshot.pipeline_commands) / counter_as_f64(snapshot.pipeline_batches);
        lines.push(Line::from(vec![
            "Pipeline: ".fg(styles::LABEL),
            format!(
                "{} batches (avg {:.1} cmds)",
                format_count(snapshot.pipeline_batches),
                avg_batch
            )
            .fg(styles::VALUE_INFO),
        ]));
    }

    // Show backend pipeline multiplexing stats if any requests have been queued
    if snapshot.pipeline_requests_queued > 0 {
        lines.push(Line::from(vec![
            "Mux Queue: ".fg(styles::LABEL),
            format!(
                "{} queued, {} completed",
                format_count(snapshot.pipeline_requests_queued),
                format_count(snapshot.pipeline_requests_completed)
            )
            .fg(styles::VALUE_INFO),
        ]));
    }

    // Add buffer stats in details mode if available
    if show_details && let Some(pool) = buffer_pool {
        let (_available, in_use, total) = pool.stats();
        let utilization_percent = buffer_utilization_percent(in_use, total);
        lines.push(Line::from(vec![
            "Buffers: ".fg(styles::LABEL),
            format!(
                "{}/{} ({:.0}%)",
                format_count(in_use as u64),
                format_count(total as u64),
                utilization_percent
            )
            .fg(load_percentage_color(utilization_percent)),
        ]));
    }

    Paragraph::new(lines)
        .block(bordered_block("App", styles::BORDER_NORMAL))
        .alignment(Alignment::Left)
}

/// Create cache summary panel
fn create_cache_summary(snapshot: &crate::metrics::MetricsSnapshot) -> Paragraph<'static> {
    /// Color for hit rate (green if >50%, blue if >0%, gray otherwise)
    const fn hit_rate_color(rate: f64) -> Color {
        if rate > 50.0 {
            styles::VALUE_PRIMARY
        } else if rate > 0.0 {
            styles::VALUE_INFO
        } else {
            styles::VALUE_NEUTRAL
        }
    }

    // Check if this is hybrid cache mode
    let is_hybrid = snapshot.disk_cache.is_some();

    // Build lines based on cache type
    let lines = if is_hybrid {
        // Hybrid cache: show disk-relevant stats
        let disk = snapshot.disk_cache.as_ref().unwrap();
        vec![
            Line::from(vec![
                "Hit Rate: ".fg(styles::LABEL),
                format!("{:.1}%", snapshot.cache_hit_rate)
                    .fg(hit_rate_color(snapshot.cache_hit_rate)),
            ]),
            Line::from(vec![
                "Disk Written: ".fg(styles::LABEL),
                format_bytes(disk.bytes_written).fg(magnitude_color(disk.bytes_written)),
            ]),
            Line::from(vec![
                "Disk Read: ".fg(styles::LABEL),
                format_bytes(disk.bytes_read).fg(magnitude_color(disk.bytes_read)),
            ]),
            Line::from(vec![
                "Disk Hits: ".fg(styles::LABEL),
                format!(
                    "{} ({:.1}%)",
                    format_count(disk.disk_hits),
                    disk.disk_hit_rate
                )
                .fg(magnitude_color(disk.disk_hits)),
            ]),
            Line::from(vec![
                "Write I/Os: ".fg(styles::LABEL),
                format_count(disk.write_ios).fg(magnitude_color(disk.write_ios)),
            ]),
        ]
    } else {
        // Memory-only cache: show memory stats
        vec![
            Line::from(vec![
                "Entries: ".fg(styles::LABEL),
                format_count(snapshot.cache_entries).fg(magnitude_color(snapshot.cache_entries)),
            ]),
            Line::from(vec![
                "Size: ".fg(styles::LABEL),
                format_bytes(snapshot.cache_size_bytes).fg(styles::VALUE_NEUTRAL),
            ]),
            Line::from(vec![
                "Hit Rate: ".fg(styles::LABEL),
                format!("{:.1}%", snapshot.cache_hit_rate)
                    .fg(hit_rate_color(snapshot.cache_hit_rate)),
            ]),
        ]
    };

    let title = if is_hybrid { "Cache (Hybrid)" } else { "Cache" };

    Paragraph::new(lines)
        .block(bordered_block(title, styles::BORDER_NORMAL))
        .alignment(Alignment::Left)
}

/// Create data transfer summary panel
fn create_transfer_summary<'a>(
    snapshot: &crate::metrics::MetricsSnapshot,
    client_to_backend: &'a str,
    backend_to_client: &'a str,
) -> Paragraph<'a> {
    Paragraph::new(vec![
        Line::from(vec![
            "Client → Backend: ".fg(styles::LABEL),
            client_to_backend.fg(styles::VALUE_SECONDARY),
        ]),
        Line::from(vec![
            "Backend → Client: ".fg(styles::LABEL),
            backend_to_client.fg(styles::VALUE_PRIMARY).bold(),
        ]),
        Line::from(vec![
            "Total: ".fg(styles::LABEL),
            format_bytes(snapshot.total_bytes()).fg(styles::VALUE_PRIMARY),
        ]),
    ])
    .block(bordered_block("Data Transfer", styles::BORDER_NORMAL))
    .alignment(Alignment::Left)
}

// ============================================================================
// Backend List Rendering Helpers
// ============================================================================

/// Create header line: health icon, server name, error rate
fn backend_header_line<'a>(
    health_icon: &'a str,
    health_color: Color,
    server_name: &'a str,
    error_text: String,
    error_rate: f64,
) -> Line<'a> {
    Line::from(vec![
        health_icon.fg(health_color).bold(),
        " ".into(),
        server_name.fg(Color::White).bold(),
        error_text.fg(error_rate_color(error_rate)),
    ])
}

/// Create address line: host:port with optional traffic share
fn backend_address_line(host: &str, port: u16, traffic_share: Option<f64>) -> Line<'static> {
    let mut spans: Vec<Span> = vec!["  ".into(), format!("{host}:{port}").fg(styles::LABEL)];

    if let Some(share) = traffic_share {
        spans.push(format!(" ({share:.1}% share)").fg(Color::Cyan));
    }

    Line::from(spans)
}

/// Create metrics line: connections, cmd/s, TTFB
fn backend_metrics_line(
    active: usize,
    max: usize,
    cmd_per_sec: String,
    ttfb: String,
) -> Line<'static> {
    Line::from(vec![
        "  Used/Max: ".fg(styles::LABEL),
        format!(
            "{}/{}",
            format_count(active as u64),
            format_count(max as u64)
        )
        .fg(magnitude_color(max as u64)),
        " | Cmd/s: ".fg(styles::LABEL),
        cmd_per_sec.fg(styles::VALUE_INFO),
        " | TTFB: ".fg(styles::LABEL),
        ttfb.fg(styles::VALUE_INFO),
    ])
}

/// Create transfer line: bytes sent/received with arrows
fn backend_transfer_line(sent: u64, received: u64) -> Line<'static> {
    Line::from(vec![
        format!("  {} ", text::ARROW_UP).fg(styles::VALUE_PRIMARY),
        format_bytes(sent).fg(styles::VALUE_PRIMARY),
        format!("  {} ", text::ARROW_DOWN).fg(styles::VALUE_NEUTRAL),
        format_bytes(received).fg(styles::VALUE_NEUTRAL),
    ])
}

/// Create article stats line: average size and count
fn backend_article_line(avg_size: String, count: u64) -> Line<'static> {
    Line::from(vec![
        "  Avg Article: ".fg(styles::LABEL),
        avg_size.fg(styles::VALUE_INFO),
        " | Articles: ".fg(styles::LABEL),
        format_count(count).fg(magnitude_color(count)),
    ])
}

/// Create error stats line: 4xx/5xx errors and connection failures
fn backend_error_line(
    errors_4xx: u64,
    errors_5xx: u64,
    _has_errors: bool,
    failures: u64,
) -> Line<'static> {
    Line::from(vec![
        "  Errors: ".fg(styles::LABEL),
        format!(
            "4xx:{} 5xx:{}",
            format_count(errors_4xx),
            format_count(errors_5xx)
        )
        .fg(backend_error_count_color(errors_4xx, errors_5xx)),
        " | Conn Fails: ".fg(styles::LABEL),
        format_count(failures).fg(connection_failures_color(failures)),
    ])
}

/// Create details line: pending, load ratio, stateful connections
fn backend_details_line(pending: usize, load_ratio: Option<f64>, stateful: usize) -> Line<'static> {
    let mut spans: Vec<Span> = vec![
        "  Load: ".fg(styles::LABEL),
        format!("{} in-flight", format_count(pending as u64)).fg(magnitude_color(pending as u64)),
    ];

    if let Some(ratio) = load_ratio {
        let ratio_percent = ratio * 100.0;
        spans.push(format!(" ({ratio_percent:.0}%)").fg(load_percentage_color(ratio_percent)));
    }

    if stateful > 0 {
        spans.push(
            format!(" | Stateful: {}", format_count(stateful as u64))
                .fg(magnitude_color(stateful as u64)),
        );
    }

    Line::from(spans)
}

/// Render list of backend servers with their stats
fn render_backend_list(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::Server],
    app: &crate::tui::TuiApp,
) {
    let items: Vec<ListItem> = snapshot
        .backend_stats
        .iter()
        .zip(servers.iter())
        .enumerate()
        .map(|(i, (stats, server))| {
            let (health_icon, health_color) = health_indicator(stats.health_status);
            let error_rate = stats.error_rate_percent();

            // Format dynamic text values
            let cmd_per_sec = app
                .latest_backend_throughput(i)
                .and_then(super::app::ThroughputPoint::commands_per_sec)
                .map_or_else(
                    || text::DEFAULT_CMD_RATE.to_string(),
                    |cps| format!("{:.0}", cps.get()),
                );

            let ttfb = stats
                .average_ttfb_ms()
                .map_or_else(|| "N/A".to_string(), |ms| format!("{ms:.1}ms"));

            let avg_size = stats
                .average_article_size()
                .map_or_else(|| "N/A".to_string(), format_bytes);

            // Build base content lines
            let mut content = vec![
                backend_header_line(
                    health_icon,
                    health_color,
                    server.name.as_str(),
                    format_error_rate(error_rate),
                    error_rate,
                ),
                backend_address_line(
                    &server.host,
                    server.port.get(),
                    app.backend_traffic_share(i),
                ),
                backend_metrics_line(
                    stats.active_connections.get(),
                    server.max_connections.get(),
                    cmd_per_sec,
                    ttfb,
                ),
                backend_transfer_line(stats.bytes_sent.as_u64(), stats.bytes_received.as_u64()),
                backend_article_line(avg_size, stats.article_count.get()),
                backend_error_line(
                    stats.errors_4xx.get(),
                    stats.errors_5xx.get(),
                    !stats.errors.is_zero(),
                    stats.connection_failures.get(),
                ),
            ];

            // Add details line in details mode
            if app.show_details() {
                content.push(backend_details_line(
                    app.backend_pending_count(i),
                    app.backend_load_ratio(i),
                    app.backend_stateful_count(i),
                ));
            }

            ListItem::new(content)
        })
        .collect();

    let list = List::new(items).block(bordered_block("Backend Servers", styles::BORDER_NORMAL));

    f.render_widget(list, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_error_count_color_highlights_any_error() {
        assert_eq!(backend_error_count_color(0, 0), styles::VALUE_NEUTRAL);
        assert_eq!(backend_error_count_color(1, 0), Color::Yellow);
        assert_eq!(backend_error_count_color(0, 1), Color::Red);
    }

    #[test]
    fn test_connection_failures_color_highlights_nonzero_failures() {
        assert_eq!(connection_failures_color(0), styles::VALUE_NEUTRAL);
        assert_eq!(connection_failures_color(1), Color::Red);
    }

    #[test]
    fn test_buffer_utilization_percent_handles_zero_total() {
        assert_eq!(buffer_utilization_percent(0, 0), 0.0);
        assert_eq!(buffer_utilization_percent(9, 10), 90.0);
    }
}

/// Render data flow visualization as line graphs
fn render_data_flow(f: &mut Frame, area: Rect, servers: &[crate::config::Server], app: &TuiApp) {
    // Build chart data in single pass (no nested loops)
    let (chart_data, max_throughput) = build_chart_data(servers, app);

    // Calculate chart bounds (extracted for testing)
    let max_throughput_rounded = calculate_chart_bounds(max_throughput);
    let max_label = format_throughput_label(max_throughput_rounded);

    // Build datasets directly from pre-computed chart data
    let datasets: Vec<Dataset> = chart_data
        .iter()
        .flat_map(|data| {
            let mut ds = Vec::with_capacity(2);

            // Sent data (upload to backend)
            if !data.sent_points_as_tuples().is_empty() {
                ds.push(
                    Dataset::default()
                        .name(format!("{} {}", data.name, text::ARROW_UP))
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(data.color))
                        .data(data.sent_points_as_tuples()),
                );
            }

            // Received data (download from backend)
            if !data.recv_points_as_tuples().is_empty() {
                ds.push(
                    Dataset::default()
                        .name(format!("{} {}", data.name, text::ARROW_DOWN))
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(data.color).add_modifier(Modifier::BOLD))
                        .data(data.recv_points_as_tuples()),
                );
            }

            ds
        })
        .collect();

    // Build and render chart
    let chart = Chart::new(datasets)
        .block(bordered_block(chart::TITLE, styles::BORDER_NORMAL))
        .x_axis(
            Axis::default()
                .title("")
                .style(Style::default().fg(styles::LABEL))
                .bounds([0.0, chart::HISTORY_POINTS])
                .labels(vec![
                    Line::from(chart::X_LABEL_15S),
                    Line::from(chart::X_LABEL_10S),
                    Line::from(chart::X_LABEL_5S),
                    Line::from(chart::X_LABEL_0S),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("Throughput")
                .style(Style::default().fg(styles::LABEL))
                .bounds([0.0, max_throughput_rounded])
                .labels(vec![
                    Line::from(chart::Y_LABEL_ZERO),
                    Line::from(format_throughput_label(max_throughput_rounded / 2.0)),
                    Line::from(max_label),
                ]),
        );

    f.render_widget(chart, area);
}

/// Render footer with help text
fn render_footer(f: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        "Press ".fg(styles::LABEL),
        "q".fg(styles::VALUE_INFO).bold(),
        " or ".fg(styles::LABEL),
        "Esc".fg(styles::VALUE_INFO).bold(),
        " to exit  |  ".fg(styles::LABEL),
        "L".fg(styles::VALUE_INFO).bold(),
        " to toggle logs  |  ".fg(styles::LABEL),
        "d".fg(styles::VALUE_INFO).bold(),
        " for details  |  ".fg(styles::LABEL),
        "Ctrl+C".fg(styles::VALUE_INFO).bold(),
        " to shutdown".fg(styles::LABEL),
    ]))
    .block(bordered_block("", styles::LABEL))
    .alignment(Alignment::Center);

    f.render_widget(footer, area);
}

/// Render recent log messages
fn render_logs(f: &mut Frame, area: Rect, app: &TuiApp) {
    let log_buffer = app.log_buffer();
    let details = app.show_details();
    let visible_lines = area.height.saturating_sub(2) as usize;
    let fetch_count = if details {
        visible_lines * 3
    } else {
        visible_lines
    };

    let text = log_buffer
        .with_recent_lines(fetch_count, |lines, skip| {
            lines.iter().skip(skip).cloned().collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .join("\n");

    let mut paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::Gray))
        .block(bordered_block(" Recent Logs ", styles::BORDER_ACTIVE));

    if details {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }

    f.render_widget(paragraph, area);
}

/// Render per-user statistics panel
fn render_user_stats(f: &mut Frame, area: Rect, snapshot: &crate::metrics::MetricsSnapshot) {
    /// Truncate username to fit display width
    fn format_username(username: &str) -> String {
        const MAX_LEN: usize = 12;
        const TRUNCATE_AT: usize = 9;
        if username.len() > MAX_LEN {
            let truncated: String = username.chars().take(TRUNCATE_AT).collect();
            format!("{truncated}...")
        } else {
            format!("{username:<MAX_LEN$}")
        }
    }

    /// Create user stat lines
    fn user_stat_lines(user: &crate::metrics::UserStats, sparkline: String) -> Vec<Line<'static>> {
        vec![
            Line::from(vec![
                format_username(&user.username).fg(Color::Cyan),
                " ".into(),
                sparkline.fg(Color::Blue),
                " ".into(),
                format!("{:>5}", format_count(user.active_connections as u64))
                    .fg(magnitude_color(user.active_connections as u64)),
            ]),
            Line::from(vec![
                "  ↑".into(),
                format!("{:>8}", format_bytes(user.bytes_sent.as_u64()))
                    .fg(magnitude_color(user.bytes_sent.as_u64())),
                "  ↓".into(),
                format!("{:>8}", format_bytes(user.bytes_received.as_u64()))
                    .fg(magnitude_color(user.bytes_received.as_u64())),
            ]),
            Line::from(vec![
                "  Rate: ".into(),
                format!("↑{}/s", format_bytes(user.bytes_sent_per_sec.get())).fg(Color::Cyan),
                " ".into(),
                format!("↓{}/s", format_bytes(user.bytes_received_per_sec.get())).fg(Color::Yellow),
            ]),
        ]
    }

    // Functional pipeline: sort → take top 10 → find max → build items
    let mut sorted_users = snapshot.user_stats.iter().collect::<Vec<_>>();
    sorted_users.sort_by_key(|u| std::cmp::Reverse(u.total_bytes()));

    let top_users: Vec<_> = sorted_users.into_iter().take(10).collect();
    let max_total = top_users.iter().map(|u| u.total_bytes()).max().unwrap_or(1);

    // Header row
    let header = ListItem::new(Line::from(vec![
        "User".fg(Color::Yellow).bold(),
        "  ".into(),
        "Bandwidth".fg(Color::Yellow).bold(),
        "       ".into(),
        "Conns".fg(Color::Yellow).bold(),
    ]));

    // User rows - functional map
    let user_items: Vec<ListItem> = top_users
        .into_iter()
        .map(|user| {
            let sparkline = create_sparkline(user.total_bytes(), max_total);
            ListItem::new(user_stat_lines(user, sparkline))
        })
        .collect();

    // Combine header + users
    let items = std::iter::once(header)
        .chain(user_items)
        .collect::<Vec<_>>();

    let list = List::new(items).block(bordered_block(" Top Users ", styles::BORDER_ACTIVE));

    f.render_widget(list, area);
}
