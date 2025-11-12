//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::app::TuiApp;
use crate::tui::constants::{chart, layout, status, styles, text};
use crate::tui::helpers::{build_chart_data, format_throughput_label, round_up_throughput};
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
    let snapshot = app.snapshot();
    let servers = app.servers();

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(layout::main_sections())
        .split(f.area());

    // Render each section
    render_title(f, chunks[0], snapshot);
    render_summary(f, chunks[1], app);
    render_backends(f, chunks[2], snapshot, servers, app);
    render_footer(f, chunks[3]);
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
    // Get latest throughput from history (functional approach)
    let (client_to_backend_str, backend_to_client_str) = app
        .latest_client_throughput()
        .map(|point| {
            (
                format!("{}{}", text::ARROW_UP, point.sent_per_sec()),
                format!("{}{}", text::ARROW_DOWN, point.received_per_sec()),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("{}{}", text::ARROW_UP, text::DEFAULT_THROUGHPUT),
                format!("{}{}", text::ARROW_DOWN, text::DEFAULT_THROUGHPUT),
            )
        });

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Client → Backend: ", Style::default().fg(styles::LABEL)),
            Span::styled(
                &client_to_backend_str,
                Style::default().fg(styles::VALUE_SECONDARY),
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
    // Split into two columns: backend list and data flow chart
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(layout::backend_columns())
        .split(area);

    render_backend_list(f, chunks[0], snapshot, servers, app);
    render_data_flow(f, chunks[1], servers, app);
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
            let status_color = if stats.active_connections > 0 {
                status::ACTIVE
            } else {
                status::INACTIVE
            };

            let error_indicator = if stats.errors > 0 {
                format!("{}{}", text::WARNING_ICON, stats.errors)
            } else {
                String::new()
            };

            // Get latest commands/sec from throughput history
            let cmd_per_sec = app
                .latest_backend_throughput(i)
                .and_then(|point| point.commands_per_sec())
                .map(|cps| cps.to_string())
                .unwrap_or_else(|| String::from(text::DEFAULT_CMD_RATE));

            let content = vec![
                Line::from(vec![
                    Span::styled(text::STATUS_INDICATOR, Style::default().fg(status_color)),
                    Span::styled(
                        server.name.as_str(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(error_indicator, Style::default().fg(status::ERROR)),
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
                    Span::styled(" clients  |  Cmd/s: ", Style::default().fg(styles::LABEL)),
                    Span::styled(cmd_per_sec, Style::default().fg(styles::VALUE_INFO)),
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

    // Apply minimum threshold and round for nice axis labels
    let max_throughput_clamped = max_throughput.max(chart::MIN_THROUGHPUT);
    let max_throughput_rounded = round_up_throughput(max_throughput_clamped);
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
