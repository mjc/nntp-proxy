//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::app::TuiApp;
use crate::tui::constants::{chart, layout, styles, text};
use crate::tui::helpers::{
    build_chart_data, calculate_chart_bounds, connection_failure_color, create_sparkline,
    error_count_color, error_rate_color, format_error_rate, format_summary_throughput,
    format_throughput_label, health_indicator, load_percentage_color, pending_count_color,
};
use ratatui::{
    Frame,
    layout::{Alignment, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, List, ListItem, Paragraph},
};

// ============================================================================
// Widget Creation Helpers (Pure Functions)
// ============================================================================

/// Create a styled span with given text, color, and optional modifiers
#[inline]
fn styled_span(text: &'static str, color: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(color))
}

/// Create a bold styled span
#[inline]
fn bold_span(text: &'static str, color: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(color).add_modifier(Modifier::BOLD))
}

/// Create a bordered block with title and color
#[inline]
fn bordered_block(title: &'static str, border_color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color))
}

/// Create a label-value span pair (common pattern)
#[inline]
fn label_value_spans(label: &'static str, value: String, value_color: Color) -> Vec<Span<'static>> {
    vec![
        styled_span(label, styles::LABEL),
        Span::styled(value, Style::default().fg(value_color)),
    ]
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
        use ratatui::layout::Constraint;
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
    const MIN_HEIGHT_FOR_LOGS: u16 = 40;
    const LOG_WINDOW_HEIGHT: u16 = 10;
    let show_logs = f.area().height >= MIN_HEIGHT_FOR_LOGS;

    // Create main layout - dynamically add log section based on available space
    use ratatui::layout::Constraint;
    let chunks = if show_logs {
        Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(layout::TITLE_HEIGHT),
                Constraint::Length(layout::SUMMARY_HEIGHT),
                Constraint::Min(layout::MIN_CHART_HEIGHT),
                Constraint::Length(LOG_WINDOW_HEIGHT),
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
        bold_span("NNTP Proxy ", styles::BORDER_ACTIVE),
        styled_span("- Real-Time Metrics Dashboard", Color::White),
    ]);

    let info_line = Line::from(
        [
            vec![
                styled_span("Uptime: ", styles::LABEL),
                Span::styled(
                    snapshot.format_uptime(),
                    Style::default()
                        .fg(styles::VALUE_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ],
            label_value_spans(
                "  |  Active: ",
                format!("{} connections", snapshot.active_connections),
                styles::VALUE_SECONDARY,
            ),
            label_value_spans(
                "  |  Total: ",
                format!("{} connections", snapshot.total_connections),
                styles::VALUE_NEUTRAL,
            ),
        ]
        .concat(),
    );

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
    use ratatui::layout::Constraint;
    let summary_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(area);

    // Left: App summary (uptime, sessions)
    let left_summary = create_app_summary(snapshot, system_stats);

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

/// Create app summary panel (uptime, CPU, memory)
fn create_app_summary(
    snapshot: &crate::metrics::MetricsSnapshot,
    system_stats: &crate::tui::SystemStats,
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

    Paragraph::new(vec![
        Line::from(label_value_spans(
            "Uptime: ",
            snapshot.format_uptime(),
            styles::VALUE_PRIMARY,
        )),
        Line::from(label_value_spans(
            "Stateful Sessions: ",
            format!("{}", snapshot.stateful_sessions),
            session_color(snapshot.stateful_sessions),
        )),
        Line::from(label_value_spans(
            "CPU: ",
            format!("{:.1}%", system_stats.cpu_usage),
            cpu_color(system_stats.cpu_usage),
        )),
        Line::from(label_value_spans(
            "Memory: ",
            format_bytes(system_stats.memory_bytes),
            styles::VALUE_INFO,
        )),
    ])
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

    Paragraph::new(vec![
        Line::from(label_value_spans(
            "Entries: ",
            format!("{}", snapshot.cache_entries),
            entries_color(snapshot.cache_entries),
        )),
        Line::from(label_value_spans(
            "Size: ",
            format_bytes(snapshot.cache_size_bytes),
            styles::VALUE_NEUTRAL,
        )),
        Line::from(label_value_spans(
            "Hit Rate: ",
            format!("{:.1}%", snapshot.cache_hit_rate),
            hit_rate_color(snapshot.cache_hit_rate),
        )),
    ])
    .block(bordered_block("Cache", styles::BORDER_NORMAL))
    .alignment(Alignment::Left)
}

/// Create data transfer summary panel
fn create_transfer_summary<'a>(
    snapshot: &crate::metrics::MetricsSnapshot,
    client_to_backend: &'a str,
    backend_to_client: &'a str,
) -> Paragraph<'a> {
    Paragraph::new(vec![
        Line::from(label_value_spans(
            "Client → Backend: ",
            client_to_backend.to_string(),
            styles::VALUE_SECONDARY,
        )),
        Line::from(vec![
            styled_span("Backend → Client: ", styles::LABEL),
            Span::styled(
                backend_to_client,
                Style::default()
                    .fg(styles::VALUE_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(label_value_spans(
            "Total: ",
            format_bytes(snapshot.total_bytes()),
            styles::VALUE_PRIMARY,
        )),
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
        Span::styled(
            health_icon,
            Style::default()
                .fg(health_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            server_name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            error_text,
            Style::default().fg(error_rate_color(error_rate)),
        ),
    ])
}

/// Create address line: host:port with optional traffic share
fn backend_address_line(host: &str, port: u16, traffic_share: Option<f64>) -> Line<'static> {
    let mut spans = vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{}:{}", host, port),
            Style::default().fg(styles::LABEL),
        ),
    ];

    if let Some(share) = traffic_share {
        spans.push(Span::styled(
            format!(" ({:.1}% share)", share),
            Style::default().fg(Color::Cyan),
        ));
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
        Span::styled("  Used/Max: ", Style::default().fg(styles::LABEL)),
        Span::styled(
            format!("{}/{}", active, max),
            Style::default().fg(styles::VALUE_SECONDARY),
        ),
        Span::styled(" | Cmd/s: ", Style::default().fg(styles::LABEL)),
        Span::styled(cmd_per_sec, Style::default().fg(styles::VALUE_INFO)),
        Span::styled(" | TTFB: ", Style::default().fg(styles::LABEL)),
        Span::styled(ttfb, Style::default().fg(styles::VALUE_INFO)),
    ])
}

/// Create transfer line: bytes sent/received with arrows
fn backend_transfer_line(sent: u64, received: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {} ", text::ARROW_UP),
            Style::default().fg(styles::VALUE_PRIMARY),
        ),
        Span::styled(
            format_bytes(sent),
            Style::default().fg(styles::VALUE_PRIMARY),
        ),
        Span::styled(
            format!("  {} ", text::ARROW_DOWN),
            Style::default().fg(styles::VALUE_NEUTRAL),
        ),
        Span::styled(
            format_bytes(received),
            Style::default().fg(styles::VALUE_NEUTRAL),
        ),
    ])
}

/// Create article stats line: average size and count
fn backend_article_line(avg_size: String, count: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled("  Avg Article: ", Style::default().fg(styles::LABEL)),
        Span::styled(avg_size, Style::default().fg(styles::VALUE_INFO)),
        Span::styled(" | Articles: ", Style::default().fg(styles::LABEL)),
        Span::styled(
            format!("{}", count),
            Style::default().fg(styles::VALUE_NEUTRAL),
        ),
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
        Span::styled("  Errors: ", Style::default().fg(styles::LABEL)),
        Span::styled(
            format!("4xx:{} 5xx:{}", errors_4xx, errors_5xx),
            Style::default().fg(error_count_color(has_errors)),
        ),
        Span::styled(" | Conn Fails: ", Style::default().fg(styles::LABEL)),
        Span::styled(
            format!("{}", failures),
            Style::default().fg(connection_failure_color(failures)),
        ),
    ])
}

/// Create details line: pending, load ratio, stateful connections
fn backend_details_line(pending: usize, load_ratio: Option<f64>, stateful: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled("  Load: ", Style::default().fg(styles::LABEL)),
        Span::styled(
            format!("{} in-flight", pending),
            Style::default().fg(pending_count_color(pending)),
        ),
    ];

    if let Some(ratio) = load_ratio {
        let ratio_percent = ratio * 100.0;
        spans.push(Span::styled(
            format!(" ({:.0}%)", ratio_percent),
            Style::default().fg(load_percentage_color(ratio_percent)),
        ));
    }

    if stateful > 0 {
        spans.push(Span::styled(
            format!(" | Stateful: {}", stateful),
            Style::default().fg(Color::Cyan),
        ));
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
                .and_then(|p| p.commands_per_sec())
                .map_or_else(
                    || text::DEFAULT_CMD_RATE.to_string(),
                    |cps| format!("{:.0}", cps.get()),
                );

            let ttfb = stats
                .average_ttfb_ms()
                .map_or_else(|| "N/A".to_string(), |ms| format!("{:.1}ms", ms));

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

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Backend Servers")
            .border_style(Style::default().fg(styles::BORDER_NORMAL)),
    );

    f.render_widget(list, area);
}

/// Render data flow visualization as line graphs
fn render_data_flow(f: &mut Frame, area: Rect, servers: &[crate::config::Server], app: &TuiApp) {
    // Build chart data in single pass (no nested loops)
    let (chart_data, max_throughput) = build_chart_data(servers, app);

    // Calculate chart bounds (extracted for testing)
    let max_throughput_rounded = calculate_chart_bounds(max_throughput);
    let max_label = format_throughput_label(max_throughput_rounded);

    // Build datasets from pre-computed chart data
    // Convert points to tuples first (must be kept alive for Dataset references)
    let chart_tuples: Vec<_> = chart_data
        .iter()
        .map(|data| {
            (
                data.sent_points_as_tuples(),
                data.recv_points_as_tuples(),
                &data.name,
                data.color,
            )
        })
        .collect();

    let datasets: Vec<Dataset> = chart_tuples
        .iter()
        .flat_map(|(sent_tuples, recv_tuples, name, color)| {
            let mut ds = Vec::with_capacity(2);

            // Sent data (upload to backend)
            if !sent_tuples.is_empty() {
                ds.push(
                    Dataset::default()
                        .name(format!("{} {}", name, text::ARROW_UP))
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(*color))
                        .data(sent_tuples),
                );
            }

            // Received data (download from backend)
            if !recv_tuples.is_empty() {
                ds.push(
                    Dataset::default()
                        .name(format!("{} {}", name, text::ARROW_DOWN))
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(*color).add_modifier(Modifier::BOLD))
                        .data(recv_tuples),
                );
            }

            ds
        })
        .collect();

    // Build and render chart
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(chart::TITLE)
                .border_style(Style::default().fg(styles::BORDER_NORMAL)),
        )
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
    /// Create a key-help span pair (key in bold info color, help in label color)
    fn key_help(key: &'static str, help: &'static str) -> Vec<Span<'static>> {
        vec![
            bold_span(key, styles::VALUE_INFO),
            styled_span(help, styles::LABEL),
        ]
    }

    let footer = Paragraph::new(Line::from(
        [
            vec![styled_span("Press ", styles::LABEL)],
            key_help("q", " or "),
            key_help("Esc", " to exit  |  "),
            key_help("L", " to toggle logs  |  "),
            key_help("d", " for details  |  "),
            key_help("Ctrl+C", " to shutdown"),
        ]
        .concat(),
    ))
    .block(bordered_block("", styles::LABEL))
    .alignment(Alignment::Center);

    f.render_widget(footer, area);
}

/// Render recent log messages
fn render_logs(f: &mut Frame, area: Rect, app: &TuiApp) {
    let log_buffer = app.log_buffer();

    // Calculate how many lines fit in the visible area (subtract 2 for borders)
    let visible_lines = area.height.saturating_sub(2) as usize;

    // Optimized: collect only the visible lines (not all 1000), then build items
    // This reduces allocations from 1000 strings to only ~8-10 visible strings
    let visible_logs = log_buffer
        .with_recent_lines(visible_lines, |lines, skip| {
            lines.iter().skip(skip).cloned().collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Build list items from visible logs (they're now owned, so no lifetime issues)
    let mut items = Vec::with_capacity(visible_logs.len());
    for line in &visible_logs {
        items
            .push(ListItem::new(Line::from(line.as_str())).style(Style::default().fg(Color::Gray)));
    }

    let list = List::new(items).block(
        Block::default()
            .title(" Recent Logs ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(styles::BORDER_ACTIVE)),
    );

    f.render_widget(list, area);
}

/// Render per-user statistics panel
fn render_user_stats(f: &mut Frame, area: Rect, snapshot: &crate::metrics::MetricsSnapshot) {
    /// Truncate username to fit display width
    fn format_username(username: &str) -> String {
        const MAX_LEN: usize = 12;
        if username.len() > MAX_LEN {
            format!("{}...", &username[..9])
        } else {
            format!("{:<MAX_LEN$}", username)
        }
    }

    /// Create user stat lines (functional pipeline)
    fn user_stat_lines(user: &crate::metrics::UserStats, sparkline: String) -> Vec<Line<'static>> {
        vec![
            Line::from(vec![
                Span::styled(
                    format_username(&user.username),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(sparkline, Style::default().fg(Color::Blue)),
                Span::raw(" "),
                Span::styled(
                    format!("{:>5}", user.active_connections),
                    Style::default().fg(Color::Green),
                ),
            ]),
            Line::from(vec![
                Span::raw("  ↑"),
                Span::styled(
                    format!("{:>8}", format_bytes(user.bytes_sent.as_u64())),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw("  ↓"),
                Span::styled(
                    format!("{:>8}", format_bytes(user.bytes_received.as_u64())),
                    Style::default().fg(Color::Magenta),
                ),
            ]),
            Line::from(vec![
                Span::raw("  Rate: "),
                Span::styled(
                    format!("↑{}/s", format_bytes(user.bytes_sent_per_sec.get())),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("↓{}/s", format_bytes(user.bytes_received_per_sec.get())),
                    Style::default().fg(Color::Yellow),
                ),
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
        bold_span("User", Color::Yellow),
        Span::raw("  "),
        bold_span("Bandwidth", Color::Yellow),
        Span::raw("       "),
        bold_span("Conns", Color::Yellow),
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
