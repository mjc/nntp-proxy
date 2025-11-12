//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::app::TuiApp;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, List, ListItem, Paragraph},
};

/// Render the main UI
pub fn render_ui(f: &mut Frame, app: &TuiApp) {
    let snapshot = app.snapshot();
    let servers = app.servers();

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Length(5), // Summary stats
            Constraint::Min(10),   // Backend visualization
            Constraint::Length(3), // Footer/help
        ])
        .split(f.area());

    // Render title
    render_title(f, chunks[0], snapshot);

    // Render summary statistics
    render_summary(f, chunks[1], snapshot);

    // Render backend data flow visualization
    render_backends(f, chunks[2], snapshot, servers);

    // Render footer
    render_footer(f, chunks[3]);
}

/// Render the title bar
fn render_title(f: &mut Frame, area: Rect, snapshot: &crate::metrics::MetricsSnapshot) {
    let title = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "NNTP Proxy ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "- Real-Time Metrics Dashboard",
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Uptime: ", Style::default().fg(Color::Gray)),
            Span::styled(
                snapshot.format_uptime(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  |  Active: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", snapshot.active_connections),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  |  Total: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", snapshot.total_connections),
                Style::default().fg(Color::Blue),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    )
    .alignment(Alignment::Center);

    f.render_widget(title, area);
}

/// Render summary statistics
fn render_summary(f: &mut Frame, area: Rect, snapshot: &crate::metrics::MetricsSnapshot) {
    let throughput = snapshot.throughput_bps();
    let throughput_str = if throughput > 1_000_000.0 {
        format!("{:.2} MB/s", throughput / 1_000_000.0)
    } else if throughput > 1_000.0 {
        format!("{:.2} KB/s", throughput / 1_000.0)
    } else {
        format!("{:.0} B/s", throughput)
    };

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Commands: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", snapshot.total_commands),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  |  ", Style::default().fg(Color::Gray)),
            Span::styled("↑ Sent: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format_bytes(snapshot.total_bytes_sent),
                Style::default().fg(Color::Green),
            ),
            Span::styled("  |  ", Style::default().fg(Color::Gray)),
            Span::styled("↓ Received: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format_bytes(snapshot.total_bytes_received),
                Style::default().fg(Color::Blue),
            ),
        ]),
        Line::from(vec![
            Span::styled("Throughput: ", Style::default().fg(Color::Gray)),
            Span::styled(
                throughput_str,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Summary")
            .border_style(Style::default().fg(Color::White)),
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
) {
    // Split into two columns: backend list and data flow chart
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Render backend list
    render_backend_list(f, chunks[0], snapshot, servers);

    // Render data flow visualization
    render_data_flow(f, chunks[1], snapshot, servers);
}

/// Render list of backend servers with their stats
fn render_backend_list(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::ServerConfig],
) {
    let items: Vec<ListItem> = snapshot
        .backend_stats
        .iter()
        .zip(servers.iter())
        .map(|(stats, server)| {
            let status_color = if stats.active_connections > 0 {
                Color::Green
            } else {
                Color::Gray
            };

            let error_indicator = if stats.errors > 0 {
                format!(" ⚠ {}", stats.errors)
            } else {
                String::new()
            };

            let content = vec![
                Line::from(vec![
                    Span::styled("● ", Style::default().fg(status_color)),
                    Span::styled(
                        format!("{}", server.name),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(error_indicator, Style::default().fg(Color::Red)),
                ]),
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("{}:{}", server.host, server.port),
                        Style::default().fg(Color::Gray),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Active: ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        format!("{}", stats.active_connections),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled("  |  Commands: ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        format!("{}", stats.total_commands),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  ↑ ", Style::default().fg(Color::Green)),
                    Span::styled(
                        format_bytes(stats.bytes_sent),
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled("  ↓ ", Style::default().fg(Color::Blue)),
                    Span::styled(
                        format_bytes(stats.bytes_received),
                        Style::default().fg(Color::Blue),
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
            .border_style(Style::default().fg(Color::White)),
    );

    f.render_widget(list, area);
}

/// Render data flow visualization as a bar chart
fn render_data_flow(
    f: &mut Frame,
    area: Rect,
    snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::ServerConfig],
) {
    // Find max bytes for scaling
    let max_bytes = snapshot
        .backend_stats
        .iter()
        .map(|s| s.bytes_sent.max(s.bytes_received))
        .max()
        .unwrap_or(1);

    // Create bar groups for each backend
    let bars: Vec<Bar> = snapshot
        .backend_stats
        .iter()
        .zip(servers.iter())
        .flat_map(|(stats, _server)| {
            // Calculate bar heights (scaled to max 100)
            let sent_height = if max_bytes > 0 {
                ((stats.bytes_sent as f64 / max_bytes as f64) * 100.0) as u64
            } else {
                0
            };
            let recv_height = if max_bytes > 0 {
                ((stats.bytes_received as f64 / max_bytes as f64) * 100.0) as u64
            } else {
                0
            };

            vec![
                Bar::default()
                    .value(sent_height)
                    .label(Line::from("↑").alignment(Alignment::Center))
                    .style(Style::default().fg(Color::Green))
                    .value_style(
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                Bar::default()
                    .value(recv_height)
                    .label(Line::from("↓").alignment(Alignment::Center))
                    .style(Style::default().fg(Color::Blue))
                    .value_style(
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
            ]
        })
        .collect();

    // Bar chart doesn't need labels in the current implementation
    // Labels are shown in the backend list on the left side

    let bar_chart = BarChart::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Data Flow (↑ Sent / ↓ Received)")
                .border_style(Style::default().fg(Color::White)),
        )
        .bar_width(3)
        .bar_gap(1)
        .group_gap(2)
        .bar_style(Style::default().fg(Color::White))
        .value_style(Style::default().add_modifier(Modifier::BOLD))
        .data(BarGroup::default().bars(&bars));

    f.render_widget(bar_chart, area);
}

/// Render footer with help text
fn render_footer(f: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("Press ", Style::default().fg(Color::Gray)),
        Span::styled(
            "q",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" or ", Style::default().fg(Color::Gray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to exit  |  ", Style::default().fg(Color::Gray)),
        Span::styled(
            "Ctrl+C",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to shutdown", Style::default().fg(Color::Gray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray)),
    )
    .alignment(Alignment::Center);

    f.render_widget(footer, area);
}
