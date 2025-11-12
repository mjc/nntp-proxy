//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::app::TuiApp;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, List, ListItem, Paragraph},
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
    render_summary(f, chunks[1], snapshot, app);

    // Render backend data flow visualization
    render_backends(f, chunks[2], snapshot, servers, app);

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
            Span::styled(" connections  |  Total: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", snapshot.total_connections),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(" connections", Style::default().fg(Color::Gray)),
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
fn render_summary(
    f: &mut Frame,
    area: Rect,
    _snapshot: &crate::metrics::MetricsSnapshot,
    app: &TuiApp,
) {
    // Get latest throughput from history (functional approach)
    let (client_to_backend_str, backend_to_client_str) = app
        .latest_client_throughput()
        .map(|point| {
            (
                format!("↑{}", point.sent_per_sec()),
                format!("↓{}", point.received_per_sec()),
            )
        })
        .unwrap_or_else(|| (String::from("↑0 B/s"), String::from("↓0 B/s")));

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Client → Backend: ", Style::default().fg(Color::Gray)),
            Span::styled(&client_to_backend_str, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("Backend → Client: ", Style::default().fg(Color::Gray)),
            Span::styled(
                &backend_to_client_str,
                Style::default()
                    .fg(Color::Green)
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
    app: &TuiApp,
) {
    // Split into two columns: backend list and data flow chart
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Render backend list
    render_backend_list(f, chunks[0], snapshot, servers, app);

    // Render data flow visualization
    render_data_flow(f, chunks[1], snapshot, servers, app);
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
                Color::Green
            } else {
                Color::Gray
            };

            let error_indicator = if stats.errors > 0 {
                format!(" ⚠ {}", stats.errors)
            } else {
                String::new()
            };

            // Get latest commands/sec from throughput history
            let cmd_per_sec = app
                .latest_backend_throughput(i)
                .and_then(|point| point.commands_per_sec())
                .map(|cps| cps.to_string())
                .unwrap_or_else(|| String::from("0.0"));

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
                    Span::styled(" clients  |  Cmd/s: ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        cmd_per_sec,
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

/// Render data flow visualization as line graphs
fn render_data_flow(
    f: &mut Frame,
    area: Rect,
    _snapshot: &crate::metrics::MetricsSnapshot,
    servers: &[crate::config::ServerConfig],
    app: &TuiApp,
) {
    // Find max throughput across all backends for scaling Y-axis
    let max_throughput = servers
        .iter()
        .enumerate()
        .flat_map(|(i, _)| app.throughput_history(i).iter())
        .flat_map(|point| {
            [
                point.sent_per_sec().get(),
                point.received_per_sec().get(),
            ]
        })
        .fold(1_000_000.0, f64::max); // Start at 1 MB/s minimum

    // Round up to next nice number (pure function)
    let round_up_throughput = |value: f64| -> f64 {
        if value > 100_000_000.0 {
            (value / 10_000_000.0).ceil() * 10_000_000.0 // Round to 10 MB/s
        } else if value > 10_000_000.0 {
            (value / 1_000_000.0).ceil() * 1_000_000.0 // Round to 1 MB/s
        } else if value > 1_000_000.0 {
            (value / 100_000.0).ceil() * 100_000.0 // Round to 100 KB/s
        } else {
            (value / 10_000.0).ceil() * 10_000.0 // Round to 10 KB/s
        }
    };

    let max_throughput_rounded = round_up_throughput(max_throughput);

    // Format Y-axis label
    let max_label = if max_throughput_rounded >= 1_000_000.0 {
        format!("{:.0} MB/s", max_throughput_rounded / 1_000_000.0)
    } else if max_throughput_rounded >= 1_000.0 {
        format!("{:.0} KB/s", max_throughput_rounded / 1_000.0)
    } else {
        format!("{:.0} B/s", max_throughput_rounded)
    };

    // Create datasets for each backend
    let mut datasets = Vec::new();
    let colors = [
        Color::Green,
        Color::Cyan,
        Color::Yellow,
        Color::Magenta,
        Color::Red,
        Color::Blue,
    ];

    // Type alias for backend chart data: (sent_bytes_points, recv_bytes_points, name, color)
    type BackendChartData<'a> = (Vec<(f64, f64)>, Vec<(f64, f64)>, &'a str, Color);

    // Collect all data points first (to satisfy lifetime requirements)
    let mut all_data: Vec<BackendChartData> = Vec::new();

    for (i, server) in servers.iter().enumerate() {
        let history = app.throughput_history(i);
        let color = colors[i % colors.len()];

        // Create data points for sent (upload to backend)
        let sent_data: Vec<(f64, f64)> = history
            .iter()
            .enumerate()
            .map(|(idx, point)| (idx as f64, point.sent_per_sec().get()))
            .collect();

        // Create data points for received (download from backend)
        let recv_data: Vec<(f64, f64)> = history
            .iter()
            .enumerate()
            .map(|(idx, point)| (idx as f64, point.received_per_sec().get()))
            .collect();

        all_data.push((sent_data, recv_data, server.name.as_str(), color));
    }

    // Now create datasets from collected data
    for (sent_data, recv_data, name, color) in &all_data {
        if !sent_data.is_empty() {
            datasets.push(
                Dataset::default()
                    .name(format!("{} ↑", name))
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(*color))
                    .data(sent_data),
            );
        }

        if !recv_data.is_empty() {
            datasets.push(
                Dataset::default()
                    .name(format!("{} ↓", name))
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(*color).add_modifier(Modifier::BOLD))
                    .data(recv_data),
            );
        }
    }

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Throughput (15s)")
                .border_style(Style::default().fg(Color::White)),
        )
        .x_axis(
            Axis::default()
                .title("")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, 60.0])
                .labels(vec![
                    Line::from("15s"),
                    Line::from("10s"),
                    Line::from("5s"),
                    Line::from("0s"),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("Throughput")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, max_throughput_rounded])
                .labels(vec![
                    Line::from("0"),
                    Line::from(format!("{:.0}", max_throughput_rounded / 2.0)),
                    Line::from(max_label),
                ]),
        );

    f.render_widget(chart, area);
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
