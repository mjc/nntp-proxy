//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::app::TuiApp;
use crate::tui::constants::{chart, layout, styles, text};
use crate::tui::helpers::{
    build_chart_data, calculate_chart_bounds, create_sparkline, format_summary_throughput,
    format_throughput_label,
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
    let title = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "NNTP Proxy ",
                Style::default()
                    .fg(styles::BORDER_ACTIVE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "- Real-Time Metrics Dashboard",
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Uptime: ", Style::default().fg(styles::LABEL)),
            Span::styled(
                snapshot.format_uptime(),
                Style::default()
                    .fg(styles::VALUE_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  |  Active: ", Style::default().fg(styles::LABEL)),
            Span::styled(
                format!("{}", snapshot.active_connections),
                Style::default()
                    .fg(styles::VALUE_SECONDARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " connections  |  Total: ",
                Style::default().fg(styles::LABEL),
            ),
            Span::styled(
                format!("{}", snapshot.total_connections),
                Style::default().fg(styles::VALUE_NEUTRAL),
            ),
            Span::styled(" connections", Style::default().fg(styles::LABEL)),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(styles::BORDER_ACTIVE)),
    )
    .alignment(Alignment::Center);

    f.render_widget(title, area);
}

/// Render summary statistics
fn render_summary(f: &mut Frame, area: Rect, app: &TuiApp) {
    let snapshot = app.snapshot();

    // Get latest throughput from history (extracted for testing)
    let (client_to_backend_str, backend_to_client_str) =
        format_summary_throughput(app.latest_client_throughput());

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Client → Backend: ", Style::default().fg(styles::LABEL)),
            Span::styled(
                &client_to_backend_str,
                Style::default().fg(styles::VALUE_SECONDARY),
            ),
            Span::styled(
                "  |  Stateful Sessions: ",
                Style::default().fg(styles::LABEL),
            ),
            Span::styled(
                format!("{}", snapshot.stateful_sessions),
                Style::default().fg(if snapshot.stateful_sessions > 0 {
                    styles::VALUE_PRIMARY
                } else {
                    styles::VALUE_NEUTRAL
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Backend → Client: ", Style::default().fg(styles::LABEL)),
            Span::styled(
                &backend_to_client_str,
                Style::default()
                    .fg(styles::VALUE_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Summary")
            .border_style(Style::default().fg(styles::BORDER_NORMAL)),
    )
    .alignment(Alignment::Left);

    f.render_widget(summary, area);
}

/// Render backend server visualization
fn render_backends(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::ServerConfig],
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

/// Render list of backend servers with their stats
fn render_backend_list(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::ServerConfig],
    app: &crate::tui::TuiApp,
) {
    let items: Vec<ListItem> = snapshot
        .backend_stats
        .iter()
        .zip(servers.iter())
        .enumerate()
        .map(|(i, (stats, server))| {
            use crate::metrics::HealthStatus;

            // Determine health status indicator
            let (health_icon, health_color) = match stats.health_status {
                HealthStatus::Healthy => ("●", Color::Green),
                HealthStatus::Degraded => ("◐", Color::Yellow),
                HealthStatus::Down => ("○", Color::Red),
            };

            // Format error rate
            let error_rate = stats.error_rate_percent();
            let error_text = if error_rate > 5.0 {
                format!(" ⚠ {:.1}%", error_rate)
            } else if error_rate > 0.0 {
                format!(" {:.1}%", error_rate)
            } else {
                String::new()
            };

            // Get latest commands/sec from throughput history
            let cmd_per_sec = app
                .latest_backend_throughput(i)
                .and_then(|point| point.commands_per_sec())
                .map(|cps| format!("{:.0}", cps.get()))
                .unwrap_or_else(|| String::from(text::DEFAULT_CMD_RATE));

            // Format TTFB (time to first byte)
            let ttfb_text = stats
                .average_ttfb_ms()
                .map(|ms| format!("{:.1}ms", ms))
                .unwrap_or_else(|| "N/A".to_string());

            // Format timing breakdown (send/recv/overhead) - only in details mode
            let timing_breakdown = if app.show_details() {
                if let (Some(send), Some(recv), Some(overhead)) = (
                    stats.average_send_ms(),
                    stats.average_recv_ms(),
                    stats.average_overhead_ms(),
                ) {
                    Some(format!(
                        " (S:{:.1} R:{:.1} O:{:.1}ms)",
                        send, recv, overhead
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            // Format average article size
            let avg_size_text = stats
                .average_article_size()
                .map(format_bytes)
                .unwrap_or_else(|| "N/A".to_string());

            let content = vec![
                Line::from(vec![
                    Span::styled(
                        health_icon,
                        Style::default()
                            .fg(health_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        server.name.as_str(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        error_text,
                        Style::default().fg(if error_rate > 5.0 {
                            Color::Red
                        } else {
                            Color::Yellow
                        }),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("{}:{}", server.host, server.port),
                        Style::default().fg(styles::LABEL),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Active: ", Style::default().fg(styles::LABEL)),
                    Span::styled(
                        format!("{}", stats.active_connections),
                        Style::default().fg(styles::VALUE_SECONDARY),
                    ),
                    Span::styled(" | Cmd/s: ", Style::default().fg(styles::LABEL)),
                    Span::styled(cmd_per_sec, Style::default().fg(styles::VALUE_INFO)),
                    Span::styled(" | TTFB: ", Style::default().fg(styles::LABEL)),
                    Span::styled(ttfb_text, Style::default().fg(styles::VALUE_INFO)),
                    // Only show timing breakdown if in details mode
                    Span::styled(
                        timing_breakdown.unwrap_or_default(),
                        Style::default().fg(styles::VALUE_SECONDARY),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!("  {} ", text::ARROW_UP),
                        Style::default().fg(styles::VALUE_PRIMARY),
                    ),
                    Span::styled(
                        format_bytes(stats.bytes_sent),
                        Style::default().fg(styles::VALUE_PRIMARY),
                    ),
                    Span::styled(
                        format!("  {} ", text::ARROW_DOWN),
                        Style::default().fg(styles::VALUE_NEUTRAL),
                    ),
                    Span::styled(
                        format_bytes(stats.bytes_received),
                        Style::default().fg(styles::VALUE_NEUTRAL),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Avg Article: ", Style::default().fg(styles::LABEL)),
                    Span::styled(avg_size_text, Style::default().fg(styles::VALUE_INFO)),
                    Span::styled(" | Errors: ", Style::default().fg(styles::LABEL)),
                    Span::styled(
                        format!("4xx:{} 5xx:{}", stats.errors_4xx, stats.errors_5xx),
                        Style::default().fg(if !stats.errors.is_zero() {
                            Color::Yellow
                        } else {
                            styles::VALUE_NEUTRAL
                        }),
                    ),
                ]),
            ];

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
fn render_data_flow(
    f: &mut Frame,
    area: Rect,
    servers: &[crate::config::ServerConfig],
    app: &TuiApp,
) {
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
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("Press ", Style::default().fg(styles::LABEL)),
        Span::styled(
            "q",
            Style::default()
                .fg(styles::VALUE_INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" or ", Style::default().fg(styles::LABEL)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(styles::VALUE_INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to exit  |  ", Style::default().fg(styles::LABEL)),
        Span::styled(
            "L",
            Style::default()
                .fg(styles::VALUE_INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to toggle logs  |  ", Style::default().fg(styles::LABEL)),
        Span::styled(
            "d",
            Style::default()
                .fg(styles::VALUE_INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" for details  |  ", Style::default().fg(styles::LABEL)),
        Span::styled(
            "Ctrl+C",
            Style::default()
                .fg(styles::VALUE_INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to shutdown", Style::default().fg(styles::LABEL)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(styles::LABEL)),
    )
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
    // Sort users by total bytes transferred (sent + received) descending
    let mut sorted_users = snapshot.user_stats.iter().collect::<Vec<_>>();
    sorted_users.sort_by(|a, b| {
        let a_total = a.bytes_sent + a.bytes_received;
        let b_total = b.bytes_sent + b.bytes_received;
        b_total.cmp(&a_total)
    });

    // Take top 10 users or all if less than 10
    let top_users: Vec<_> = sorted_users.iter().take(10).collect();

    // Find max total bytes for scaling sparkline
    let max_total = top_users
        .iter()
        .map(|u| u.bytes_sent + u.bytes_received)
        .max()
        .unwrap_or(1);

    let mut items = Vec::with_capacity(top_users.len() + 1);

    // Header
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            "User",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "Bandwidth",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("       "),
        Span::styled(
            "Conns",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])));

    // User rows with sparkline
    for user in top_users {
        let username = if user.username.len() > 12 {
            format!("{}...", &user.username[..9])
        } else {
            format!("{:<12}", user.username)
        };

        let total_bytes = user.bytes_sent + user.bytes_received;
        let bar = create_sparkline(total_bytes, max_total);

        items.push(ListItem::new(vec![
            Line::from(vec![
                Span::styled(username, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(bar, Style::default().fg(Color::Blue)),
                Span::raw(" "),
                Span::styled(
                    format!("{:>5}", user.active_connections),
                    Style::default().fg(Color::Green),
                ),
            ]),
            Line::from(vec![
                Span::raw("  ↑"),
                Span::styled(
                    format!("{:>8}", format_bytes(user.bytes_sent)),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw("  ↓"),
                Span::styled(
                    format!("{:>8}", format_bytes(user.bytes_received)),
                    Style::default().fg(Color::Magenta),
                ),
            ]),
            Line::from(vec![
                Span::raw("  Rate: "),
                Span::styled(
                    format!("↑{}/s", format_bytes(user.bytes_sent_per_sec)),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("↓{}/s", format_bytes(user.bytes_received_per_sec)),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
        ]));
    }

    let list = List::new(items).block(
        Block::default()
            .title(" Top Users ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(styles::BORDER_ACTIVE)),
    );

    f.render_widget(list, area);
}
