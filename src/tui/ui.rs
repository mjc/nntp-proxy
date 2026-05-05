//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::constants::{chart, layout, styles, text};
use crate::tui::dashboard::{BufferPoolStats, DashboardState};
use crate::tui::helpers::{
    build_chart_data, calculate_chart_bounds, connection_failure_color, create_sparkline,
    error_count_color, error_rate_color, format_error_rate, format_summary_throughput,
    format_throughput_label, health_indicator, load_percentage_color, pending_count_color,
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
pub fn render_ui(
    f: &mut Frame,
    state: &DashboardState,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
) {
    use crate::tui::app::ViewMode;

    // Check if we're in log fullscreen mode
    if state.view_mode == ViewMode::LogFullscreen {
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

        render_title(f, chunks[0], &state.snapshot);
        render_logs(f, chunks[1], state);
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
    render_title(f, chunks[0], &state.snapshot);
    render_summary(f, chunks[1], state, attached_ui_stats);

    // Backends area now contains 3 columns: backends, chart, and user stats
    render_backends(f, chunks[2], state);

    if show_logs {
        render_logs(f, chunks[3], state);
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
        format!("{} connections", snapshot.active_connections).fg(styles::VALUE_SECONDARY),
        "  |  Total: ".fg(styles::LABEL),
        format!("{} connections", snapshot.total_connections).fg(styles::VALUE_NEUTRAL),
    ]);

    let title = Paragraph::new(vec![title_line, info_line])
        .block(bordered_block("", styles::BORDER_ACTIVE))
        .alignment(Alignment::Center);

    f.render_widget(title, area);
}

/// Render summary statistics
fn render_summary(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
) {
    let snapshot = &state.snapshot;
    let system_stats = &state.system_stats;

    // Get latest throughput from history (extracted for testing)
    let (client_to_backend_str, backend_to_client_str) =
        format_summary_throughput(state.latest_client_throughput());

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
        state.buffer_pool(),
        state.show_details,
        attached_ui_stats,
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
fn render_backends(f: &mut Frame, area: Rect, state: &DashboardState) {
    // Split into three columns: backend list, data flow chart, and top users
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(layout::backend_columns())
        .split(area);

    render_backend_list(f, chunks[0], state);
    render_data_flow(f, chunks[1], state);
    render_user_stats(f, chunks[2], &state.snapshot);
}

// ============================================================================
// Summary Panel Builders (Pure Functions)
// ============================================================================

/// Create app summary panel (uptime, CPU, memory, buffer stats in details mode)
fn create_app_summary(
    snapshot: &crate::metrics::MetricsSnapshot,
    system_stats: &crate::tui::SystemStats,
    buffer_pool: Option<&BufferPoolStats>,
    show_details: bool,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
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

    /// Color for session count (highlight if active)
    const fn session_color(count: usize) -> Color {
        if count > 0 {
            styles::VALUE_PRIMARY
        } else {
            styles::VALUE_NEUTRAL
        }
    }

    /// Color for buffer pool utilization
    const fn buffer_color(in_use: usize, total: usize) -> Color {
        let percent = match (in_use * 100).checked_div(total) {
            Some(v) => v,
            None => 0,
        };
        if percent > 80 {
            Color::Red
        } else if percent > 60 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    if let Some(ui_stats) = attached_ui_stats {
        let lines = vec![
            Line::from(vec![
                "Uptime: ".fg(styles::LABEL),
                snapshot.format_uptime().fg(styles::VALUE_PRIMARY),
            ]),
            Line::from(vec![
                "Stateful Sessions: ".fg(styles::LABEL),
                format!("{}", snapshot.stateful_sessions)
                    .fg(session_color(snapshot.stateful_sessions)),
            ]),
            Line::from(vec![
                "CPU (proxy, UI): ".fg(styles::LABEL),
                format!("{:.1}%", system_stats.cpu_usage).fg(cpu_color(system_stats.cpu_usage)),
                " / ".fg(styles::LABEL),
                format!("{:.1}%", ui_stats.cpu_usage).fg(cpu_color(ui_stats.cpu_usage)),
            ]),
            Line::from(vec![
                "Memory (proxy, UI): ".fg(styles::LABEL),
                format_bytes(system_stats.memory_bytes).fg(styles::VALUE_INFO),
                " / ".fg(styles::LABEL),
                format_bytes(ui_stats.memory_bytes).fg(styles::VALUE_INFO),
            ]),
        ];

        return Paragraph::new(lines)
            .block(bordered_block("App", styles::BORDER_NORMAL))
            .alignment(Alignment::Left);
    }

    let mut lines = vec![
        Line::from(vec![
            "Uptime: ".fg(styles::LABEL),
            snapshot.format_uptime().fg(styles::VALUE_PRIMARY),
        ]),
        Line::from(vec![
            "Stateful Sessions: ".fg(styles::LABEL),
            format!("{}", snapshot.stateful_sessions).fg(session_color(snapshot.stateful_sessions)),
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
                snapshot.pipeline_batches, avg_batch
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
                snapshot.pipeline_requests_queued, snapshot.pipeline_requests_completed
            )
            .fg(styles::VALUE_INFO),
        ]));
    }

    // Add buffer stats in details mode if available
    if show_details && let Some(pool) = buffer_pool {
        let usage_percent = if pool.total > 0 {
            size_as_f64(pool.in_use * 100) / size_as_f64(pool.total)
        } else {
            0.0
        };
        lines.push(Line::from(vec![
            "Buffers: ".fg(styles::LABEL),
            format!("{}/{} ({:.0}%)", pool.in_use, pool.total, usage_percent)
                .fg(buffer_color(pool.in_use, pool.total)),
        ]));
    }

    Paragraph::new(lines)
        .block(bordered_block("App", styles::BORDER_NORMAL))
        .alignment(Alignment::Left)
}

/// Create cache summary panel
fn create_cache_summary(snapshot: &crate::metrics::MetricsSnapshot) -> Paragraph<'static> {
    /// Color for cache entries (highlight if non-empty)
    const fn entries_color(count: u64) -> Color {
        if count > 0 {
            styles::VALUE_INFO
        } else {
            styles::VALUE_NEUTRAL
        }
    }

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

    const fn non_zero_color(value: u64) -> Color {
        if value > 0 {
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
                format_bytes(disk.bytes_written).fg(non_zero_color(disk.bytes_written)),
            ]),
            Line::from(vec![
                "Disk Read: ".fg(styles::LABEL),
                format_bytes(disk.bytes_read).fg(non_zero_color(disk.bytes_read)),
            ]),
            Line::from(vec![
                "Disk Hits: ".fg(styles::LABEL),
                format!("{} ({:.1}%)", disk.disk_hits, disk.disk_hit_rate)
                    .fg(non_zero_color(disk.disk_hits)),
            ]),
            Line::from(vec![
                "Write I/Os: ".fg(styles::LABEL),
                format!("{}", disk.write_ios).fg(styles::VALUE_NEUTRAL),
            ]),
        ]
    } else {
        // Memory-only cache: show memory stats
        vec![
            Line::from(vec![
                "Entries: ".fg(styles::LABEL),
                format!("{}", snapshot.cache_entries).fg(entries_color(snapshot.cache_entries)),
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
        format!("{active}/{max}").fg(styles::VALUE_SECONDARY),
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
        format!("{count}").fg(styles::VALUE_NEUTRAL),
    ])
}

/// Create error stats line: 4xx/5xx errors and connection failures
fn backend_error_line(
    errors_4xx: u64,
    errors_5xx: u64,
    has_errors: bool,
    failures: u64,
) -> Line<'static> {
    Line::from(vec![
        "  Errors: ".fg(styles::LABEL),
        format!("4xx:{errors_4xx} 5xx:{errors_5xx}").fg(error_count_color(has_errors)),
        " | Conn Fails: ".fg(styles::LABEL),
        format!("{failures}").fg(connection_failure_color(failures)),
    ])
}

/// Create details line: pending, load ratio, stateful connections
fn backend_details_line(pending: usize, load_ratio: Option<f64>, stateful: usize) -> Line<'static> {
    let mut spans: Vec<Span> = vec![
        "  Load: ".fg(styles::LABEL),
        format!("{pending} in-flight").fg(pending_count_color(pending)),
    ];

    if let Some(ratio) = load_ratio {
        let ratio_percent = ratio * 100.0;
        spans.push(format!(" ({ratio_percent:.0}%)").fg(load_percentage_color(ratio_percent)));
    }

    if stateful > 0 {
        spans.push(format!(" | Stateful: {stateful}").fg(Color::Cyan));
    }

    Line::from(spans)
}

/// Render list of backend servers with their stats
fn render_backend_list(f: &mut Frame, area: Rect, state: &DashboardState) {
    let items: Vec<ListItem> = state
        .backend_views
        .iter()
        .enumerate()
        .map(|(i, backend)| {
            let stats = &backend.stats;
            let server = &backend.server;
            let (health_icon, health_color) = health_indicator(backend.health_status);
            let error_rate = stats.error_rate_percent();

            // Format dynamic text values
            let cmd_per_sec = backend
                .latest_throughput()
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
                    state.backend_traffic_share(i),
                ),
                backend_metrics_line(
                    backend.active_connections,
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
            if state.show_details {
                content.push(backend_details_line(
                    state.backend_pending_count(i),
                    state.backend_load_ratio(i),
                    state.backend_stateful_count(i),
                ));
            }

            ListItem::new(content)
        })
        .collect();

    let list = List::new(items).block(bordered_block("Backend Servers", styles::BORDER_NORMAL));

    f.render_widget(list, area);
}

/// Render data flow visualization as line graphs
fn render_data_flow(f: &mut Frame, area: Rect, state: &DashboardState) {
    // Build chart data in single pass (no nested loops)
    let (chart_data, max_throughput) = build_chart_data(&state.backend_views);

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
fn render_logs(f: &mut Frame, area: Rect, state: &DashboardState) {
    let details = state.show_details;
    let visible_lines = area.height.saturating_sub(2) as usize;
    let fetch_count = if details {
        visible_lines * 3
    } else {
        visible_lines
    };

    let text = recent_log_lines(&state.log_lines, fetch_count).join("\n");

    let mut paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::Gray))
        .block(bordered_block(" Recent Logs ", styles::BORDER_ACTIVE));

    if details {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }

    f.render_widget(paragraph, area);
}

fn recent_log_lines(lines: &[String], count: usize) -> &[String] {
    let start = lines.len().saturating_sub(count);
    &lines[start..]
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
                format!("{:>5}", user.active_connections).fg(Color::Green),
            ]),
            Line::from(vec![
                "  ↑".into(),
                format!("{:>8}", format_bytes(user.bytes_sent.as_u64())).fg(Color::Blue),
                "  ↓".into(),
                format!("{:>8}", format_bytes(user.bytes_received.as_u64())).fg(Color::Magenta),
            ]),
            Line::from(vec![
                "  Rate: ".into(),
                format!("↑{}/s", format_bytes(user.bytes_sent_per_sec.get())).fg(Color::Cyan),
                " ".into(),
                format!("↓{}/s", format_bytes(user.bytes_received_per_sec.get())).fg(Color::Yellow),
            ]),
        ]
    }

    let top_users = top_users_by_bytes(&snapshot.user_stats);
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

fn top_users_by_bytes(users: &[crate::metrics::UserStats]) -> Vec<&crate::metrics::UserStats> {
    let mut sorted_users = users.iter().collect::<Vec<_>>();
    sorted_users.sort_by_key(|user| std::cmp::Reverse(user.total_bytes()));
    sorted_users.into_iter().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::UserStats;

    fn user_stats(name: &str, total_bytes: u64) -> UserStats {
        UserStats {
            username: name.to_string(),
            bytes_sent: crate::types::BytesSent::new(total_bytes),
            ..Default::default()
        }
    }

    #[test]
    fn recent_log_lines_returns_full_slice_when_count_exceeds_length() {
        let lines = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let recent = recent_log_lines(&lines, 10);

        assert_eq!(recent, lines.as_slice());
    }

    #[test]
    fn recent_log_lines_returns_tail_slice() {
        let lines = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
        ];

        let recent = recent_log_lines(&lines, 2);

        assert_eq!(recent, &lines[2..]);
    }

    #[test]
    fn recent_log_lines_handles_zero_count() {
        let lines = vec!["one".to_string(), "two".to_string()];

        let recent = recent_log_lines(&lines, 0);

        assert!(recent.is_empty());
    }

    #[test]
    fn top_users_by_bytes_sorts_descending() {
        let users = vec![
            user_stats("alice", 10),
            user_stats("bob", 30),
            user_stats("carol", 20),
        ];

        let top_users = top_users_by_bytes(&users);

        let usernames = top_users
            .iter()
            .map(|user| user.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(usernames, vec!["bob", "carol", "alice"]);
    }

    #[test]
    fn top_users_by_bytes_caps_results_at_ten() {
        let users = (0..12)
            .map(|i| user_stats(&format!("user{i}"), i))
            .collect::<Vec<_>>();

        let top_users = top_users_by_bytes(&users);

        assert_eq!(top_users.len(), 10);
        assert_eq!(top_users[0].username, "user11");
        assert_eq!(top_users[9].username, "user2");
    }
}
