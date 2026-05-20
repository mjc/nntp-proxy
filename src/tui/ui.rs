//! TUI rendering and layout

use crate::formatting::format_bytes;
use crate::tui::RemoteDashboardStatus;
use crate::tui::constants::{chart, layout, styles, text};
use crate::tui::dashboard::{
    BufferPoolStats, DashboardMetrics, DashboardState, DashboardUserStats,
};
use crate::tui::helpers::{
    build_chart_data, calculate_chart_bounds, connection_failure_color, create_sparkline,
    error_count_color, error_rate_color, format_error_rate, format_summary_throughput,
    format_throughput_label, health_indicator,
};
use arrayvec::ArrayString;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Chart, Dataset, GraphType, List, ListItem, Paragraph, Wrap},
};
use std::fmt::Write as _;

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

#[inline]
fn untitled_bordered_block(border_color: Color) -> Block<'static> {
    Block::bordered().border_style(Style::new().fg(border_color))
}

#[derive(Debug, Clone)]
pub(crate) struct AttachedRenderCache {
    title_lines: Vec<Line<'static>>,
}

pub(crate) fn build_attached_render_cache(
    state: &DashboardState,
    remote_status: &RemoteDashboardStatus,
) -> AttachedRenderCache {
    AttachedRenderCache {
        title_lines: build_title_lines(&state.metrics, Some(remote_status)),
    }
}

// ============================================================================
// Render Functions
// ============================================================================

/// Render the main UI
pub(crate) fn render_ui(
    f: &mut Frame,
    state: &DashboardState,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
    remote_status: Option<&RemoteDashboardStatus>,
    view_mode_override: Option<crate::tui::app::ViewMode>,
    show_details_override: Option<bool>,
) {
    let view_mode = view_mode_override.unwrap_or(state.view_mode);
    let show_details = show_details_override.unwrap_or(state.show_details);

    if let Some(chunks) = dashboard_fullscreen_chunks(view_mode, f.area()) {
        render_title(f, chunks[0], &state.metrics, remote_status);
        render_logs(f, chunks[1], &state.log_lines, show_details);
        render_footer(f, chunks[2]);
        return;
    }

    let show_logs = should_show_dashboard_logs(f.area().height);
    match dashboard_main_chunks(f.area(), show_logs) {
        DashboardMainChunks::WithLogs([title, summary, backends, logs, footer]) => {
            render_title(f, title, &state.metrics, remote_status);
            render_summary(f, summary, state, attached_ui_stats, show_details);
            render_backends(f, backends, state, show_details);
            render_logs(f, logs, &state.log_lines, show_details);
            render_footer(f, footer);
        }
        DashboardMainChunks::WithoutLogs([title, summary, backends, footer]) => {
            render_title(f, title, &state.metrics, remote_status);
            render_summary(f, summary, state, attached_ui_stats, show_details);
            render_backends(f, backends, state, show_details);
            render_footer(f, footer);
        }
    }
}

pub(crate) fn render_attached_ui(
    f: &mut Frame,
    state: &DashboardState,
    render_cache: &AttachedRenderCache,
    view_mode: crate::tui::app::ViewMode,
    show_details: bool,
) {
    if let Some(chunks) = dashboard_fullscreen_chunks(view_mode, f.area()) {
        render_title_lines(f, chunks[0], &render_cache.title_lines);
        render_logs(f, chunks[1], &state.log_lines, show_details);
        render_footer(f, chunks[2]);
        return;
    }

    let show_logs = should_show_dashboard_logs(f.area().height);
    match dashboard_main_chunks(f.area(), show_logs) {
        DashboardMainChunks::WithLogs([title, summary, backends, logs, footer]) => {
            render_title_lines(f, title, &render_cache.title_lines);
            render_summary(f, summary, state, None, show_details);
            render_backends(f, backends, state, show_details);
            render_logs(f, logs, &state.log_lines, show_details);
            render_footer(f, footer);
        }
        DashboardMainChunks::WithoutLogs([title, summary, backends, footer]) => {
            render_title_lines(f, title, &render_cache.title_lines);
            render_summary(f, summary, state, None, show_details);
            render_backends(f, backends, state, show_details);
            render_footer(f, footer);
        }
    }
}

fn should_show_dashboard_logs(area_height: u16) -> bool {
    area_height >= layout::MIN_HEIGHT_FOR_LOGS
}

enum DashboardMainChunks {
    WithLogs([Rect; 5]),
    WithoutLogs([Rect; 4]),
}

#[cfg(test)]
impl DashboardMainChunks {
    const fn len(&self) -> usize {
        match self {
            Self::WithLogs(_) => 5,
            Self::WithoutLogs(_) => 4,
        }
    }
}

fn dashboard_fullscreen_chunks(
    view_mode: crate::tui::app::ViewMode,
    area: Rect,
) -> Option<[Rect; 3]> {
    if view_mode != crate::tui::app::ViewMode::LogFullscreen {
        return None;
    }

    let inner = inset_rect(area, 1);
    Some(split_vertical_three(
        inner,
        layout::TITLE_HEIGHT,
        layout::FOOTER_HEIGHT,
    ))
}

fn dashboard_main_chunks(area: Rect, show_logs: bool) -> DashboardMainChunks {
    let inner = inset_rect(area, 1);
    if show_logs {
        DashboardMainChunks::WithLogs(split_dashboard_main_with_logs(inner))
    } else {
        DashboardMainChunks::WithoutLogs(split_dashboard_main_without_logs(inner))
    }
}

fn inset_rect(area: Rect, margin: u16) -> Rect {
    let x = area.x.saturating_add(area.width.min(margin));
    let y = area.y.saturating_add(area.height.min(margin));
    let width = area.width.saturating_sub(margin.saturating_mul(2));
    let height = area.height.saturating_sub(margin.saturating_mul(2));
    Rect::new(x, y, width, height)
}

fn split_vertical_three(area: Rect, top_height: u16, bottom_height: u16) -> [Rect; 3] {
    let top = top_height.min(area.height);
    let remaining_after_top = area.height.saturating_sub(top);
    let bottom = bottom_height.min(remaining_after_top);
    let middle = remaining_after_top.saturating_sub(bottom);

    [
        Rect::new(area.x, area.y, area.width, top),
        Rect::new(area.x, area.y.saturating_add(top), area.width, middle),
        Rect::new(
            area.x,
            area.y.saturating_add(top).saturating_add(middle),
            area.width,
            bottom,
        ),
    ]
}

fn split_dashboard_main_without_logs(area: Rect) -> [Rect; 4] {
    let title = layout::TITLE_HEIGHT.min(area.height);
    let remaining_after_title = area.height.saturating_sub(title);
    let summary = layout::SUMMARY_HEIGHT.min(remaining_after_title);
    let remaining_after_summary = remaining_after_title.saturating_sub(summary);
    let footer = layout::FOOTER_HEIGHT.min(remaining_after_summary);
    let backends = remaining_after_summary.saturating_sub(footer);
    debug_assert!(
        backends >= layout::MIN_CHART_HEIGHT
            || area.height < layout::TITLE_HEIGHT + layout::SUMMARY_HEIGHT + layout::FOOTER_HEIGHT
    );

    [
        Rect::new(area.x, area.y, area.width, title),
        Rect::new(area.x, area.y.saturating_add(title), area.width, summary),
        Rect::new(
            area.x,
            area.y.saturating_add(title).saturating_add(summary),
            area.width,
            backends,
        ),
        Rect::new(
            area.x,
            area.y
                .saturating_add(title)
                .saturating_add(summary)
                .saturating_add(backends),
            area.width,
            footer,
        ),
    ]
}

fn split_dashboard_main_with_logs(area: Rect) -> [Rect; 5] {
    let title = layout::TITLE_HEIGHT.min(area.height);
    let remaining_after_title = area.height.saturating_sub(title);
    let summary = layout::SUMMARY_HEIGHT.min(remaining_after_title);
    let remaining_after_summary = remaining_after_title.saturating_sub(summary);
    let logs = layout::LOG_WINDOW_HEIGHT.min(remaining_after_summary);
    let remaining_after_logs = remaining_after_summary.saturating_sub(logs);
    let footer = layout::FOOTER_HEIGHT.min(remaining_after_logs);
    let backends = remaining_after_logs.saturating_sub(footer);
    debug_assert!(
        backends >= layout::MIN_CHART_HEIGHT
            || area.height
                < layout::TITLE_HEIGHT
                    + layout::SUMMARY_HEIGHT
                    + layout::LOG_WINDOW_HEIGHT
                    + layout::FOOTER_HEIGHT
    );

    [
        Rect::new(area.x, area.y, area.width, title),
        Rect::new(area.x, area.y.saturating_add(title), area.width, summary),
        Rect::new(
            area.x,
            area.y.saturating_add(title).saturating_add(summary),
            area.width,
            backends,
        ),
        Rect::new(
            area.x,
            area.y
                .saturating_add(title)
                .saturating_add(summary)
                .saturating_add(backends),
            area.width,
            logs,
        ),
        Rect::new(
            area.x,
            area.y
                .saturating_add(title)
                .saturating_add(summary)
                .saturating_add(backends)
                .saturating_add(logs),
            area.width,
            footer,
        ),
    ]
}

fn split_summary_columns(area: Rect) -> [Rect; 3] {
    let first = area.width.saturating_mul(33) / 100;
    let second = area.width.saturating_mul(34) / 100;
    let third = area.width.saturating_sub(first).saturating_sub(second);

    [
        Rect::new(area.x, area.y, first, area.height),
        Rect::new(area.x.saturating_add(first), area.y, second, area.height),
        Rect::new(
            area.x.saturating_add(first).saturating_add(second),
            area.y,
            third,
            area.height,
        ),
    ]
}

fn split_backend_columns(area: Rect) -> [Rect; 3] {
    let user_stats = area.width.saturating_mul(22) / 100;
    let primary = area.width.saturating_sub(user_stats);
    let data_flow = primary.saturating_mul(56) / 100;
    let backend_list = primary.saturating_sub(data_flow);

    [
        Rect::new(area.x, area.y, backend_list, area.height),
        Rect::new(
            area.x.saturating_add(backend_list),
            area.y,
            data_flow,
            area.height,
        ),
        Rect::new(
            area.x
                .saturating_add(backend_list)
                .saturating_add(data_flow),
            area.y,
            user_stats,
            area.height,
        ),
    ]
}

/// Render the title bar
fn render_title(
    f: &mut Frame,
    area: Rect,
    metrics: &DashboardMetrics,
    remote_status: Option<&RemoteDashboardStatus>,
) {
    let block = untitled_bordered_block(styles::BORDER_ACTIVE);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.left().saturating_add(inner.width);
    let buffer = f.buffer_mut();

    let title_y = inner.top();
    let mut title_x = centered_x(inner, title_title_width(remote_status));
    write_part(
        buffer,
        &mut title_x,
        title_y,
        right,
        "NNTP Proxy ",
        Style::default()
            .fg(styles::BORDER_ACTIVE)
            .add_modifier(Modifier::BOLD),
    );
    let (suffix, suffix_style) = title_suffix(remote_status);
    write_part(buffer, &mut title_x, title_y, right, suffix, suffix_style);

    if inner.height < 2 {
        return;
    }

    let info_y = inner.top().saturating_add(1);
    render_title_info_row(buffer, inner, info_y, metrics, remote_status);
}

fn render_title_lines(f: &mut Frame, area: Rect, lines: &[Line<'_>]) {
    render_block_lines(
        f,
        area,
        untitled_bordered_block(styles::BORDER_ACTIVE),
        lines,
        Alignment::Center,
    );
}

fn centered_x(area: Rect, content_width: u16) -> u16 {
    area.left()
        .saturating_add(area.width.saturating_sub(content_width) / 2)
}

fn title_suffix(remote_status: Option<&RemoteDashboardStatus>) -> (&'static str, Style) {
    remote_status.map_or_else(
        || {
            (
                "- Real-Time Metrics Dashboard",
                Style::default().fg(Color::White),
            )
        },
        |status| match status {
            RemoteDashboardStatus::Connecting { .. } => (
                "- Attached Dashboard (connecting)",
                Style::default().fg(Color::Yellow),
            ),
            RemoteDashboardStatus::Connected { .. } => (
                "- Attached Dashboard (live)",
                Style::default().fg(styles::VALUE_PRIMARY),
            ),
            RemoteDashboardStatus::Reconnecting { .. } => (
                "- Attached Dashboard (reconnecting)",
                Style::default().fg(Color::Yellow),
            ),
        },
    )
}

fn title_title_width(remote_status: Option<&RemoteDashboardStatus>) -> u16 {
    let (suffix, _) = title_suffix(remote_status);
    u16::try_from("NNTP Proxy ".len() + suffix.len()).unwrap_or(u16::MAX)
}

fn render_title_info_row(
    buffer: &mut Buffer,
    area: Rect,
    y: u16,
    metrics: &DashboardMetrics,
    remote_status: Option<&RemoteDashboardStatus>,
) {
    let right = area.left().saturating_add(area.width);
    let mut x = area.left();

    match remote_status {
        Some(status) => render_remote_title_status_row(buffer, &mut x, y, right, status),
        None => render_local_title_status_row(buffer, &mut x, y, right, metrics),
    }
}

fn render_local_title_status_row(
    buffer: &mut Buffer,
    x: &mut u16,
    y: u16,
    right: u16,
    metrics: &DashboardMetrics,
) {
    let uptime = metrics.format_uptime();
    let active = connections_text(metrics.active_connections);
    let total = connections_text_u64(metrics.total_connections);

    write_part(
        buffer,
        x,
        y,
        right,
        "Uptime: ",
        Style::default().fg(styles::LABEL),
    );
    write_part(
        buffer,
        x,
        y,
        right,
        &uptime,
        Style::default()
            .fg(styles::VALUE_PRIMARY)
            .add_modifier(Modifier::BOLD),
    );
    write_part(
        buffer,
        x,
        y,
        right,
        "  |  Active: ",
        Style::default().fg(styles::LABEL),
    );
    write_part(
        buffer,
        x,
        y,
        right,
        &active,
        Style::default().fg(styles::VALUE_SECONDARY),
    );
    write_part(
        buffer,
        x,
        y,
        right,
        "  |  Total: ",
        Style::default().fg(styles::LABEL),
    );
    write_part(
        buffer,
        x,
        y,
        right,
        &total,
        Style::default().fg(styles::VALUE_NEUTRAL),
    );
}

fn render_remote_title_status_row(
    buffer: &mut Buffer,
    x: &mut u16,
    y: u16,
    right: u16,
    status: &RemoteDashboardStatus,
) {
    write_part(
        buffer,
        x,
        y,
        right,
        "Remote: ",
        Style::default().fg(styles::LABEL),
    );
    match status {
        RemoteDashboardStatus::Connecting { target } => {
            write_part(
                buffer,
                x,
                y,
                right,
                "connecting",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            render_target_part(buffer, x, y, right, target);
        }
        RemoteDashboardStatus::Connected { target } => {
            write_part(
                buffer,
                x,
                y,
                right,
                "live",
                Style::default()
                    .fg(styles::VALUE_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            );
            render_target_part(buffer, x, y, right, target);
        }
        RemoteDashboardStatus::Reconnecting {
            target,
            retry_delay,
            ..
        } => {
            write_part(
                buffer,
                x,
                y,
                right,
                "reconnecting",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            let mut delay = ArrayString::<24>::new();
            let _ = write!(&mut delay, " in {:.1}s", retry_delay.as_secs_f32());
            write_part(
                buffer,
                x,
                y,
                right,
                &delay,
                Style::default().fg(styles::LABEL),
            );
            render_target_part(buffer, x, y, right, target);
            write_part(
                buffer,
                x,
                y,
                right,
                "  |  Snapshot: ",
                Style::default().fg(styles::LABEL),
            );
            write_part(
                buffer,
                x,
                y,
                right,
                "last known",
                Style::default().fg(Color::Yellow),
            );
        }
    }
}

fn render_target_part(
    buffer: &mut Buffer,
    x: &mut u16,
    y: u16,
    right: u16,
    target: &std::net::SocketAddr,
) {
    write_part(
        buffer,
        x,
        y,
        right,
        "  |  Target: ",
        Style::default().fg(styles::LABEL),
    );
    let mut target_buf = ArrayString::<64>::new();
    let _ = write!(&mut target_buf, "{target}");
    write_part(
        buffer,
        x,
        y,
        right,
        &target_buf,
        Style::default().fg(styles::VALUE_INFO),
    );
}

fn connections_text(count: usize) -> ArrayString<32> {
    let mut value = ArrayString::<32>::new();
    let _ = write!(&mut value, "{count} connections");
    value
}

fn connections_text_u64(count: u64) -> ArrayString<32> {
    let mut value = ArrayString::<32>::new();
    let _ = write!(&mut value, "{count} connections");
    value
}

fn build_title_lines(
    metrics: &DashboardMetrics,
    remote_status: Option<&RemoteDashboardStatus>,
) -> Vec<Line<'static>> {
    let title_suffix = remote_status.map_or_else(
        || Span::from("- Real-Time Metrics Dashboard").fg(Color::White),
        |status| match status {
            RemoteDashboardStatus::Connecting { .. } => {
                Span::from("- Attached Dashboard (connecting)").fg(Color::Yellow)
            }
            RemoteDashboardStatus::Connected { .. } => {
                Span::from("- Attached Dashboard (live)").fg(styles::VALUE_PRIMARY)
            }
            RemoteDashboardStatus::Reconnecting { .. } => {
                Span::from("- Attached Dashboard (reconnecting)").fg(Color::Yellow)
            }
        },
    );

    let info_line = remote_status.map_or_else(
        || {
            Line::from(vec![
                "Uptime: ".fg(styles::LABEL),
                metrics.format_uptime().fg(styles::VALUE_PRIMARY).bold(),
                "  |  Active: ".fg(styles::LABEL),
                format!("{} connections", metrics.active_connections).fg(styles::VALUE_SECONDARY),
                "  |  Total: ".fg(styles::LABEL),
                format!("{} connections", metrics.total_connections).fg(styles::VALUE_NEUTRAL),
            ])
        },
        build_remote_title_status_line,
    );

    vec![
        Line::from(vec![
            "NNTP Proxy ".fg(styles::BORDER_ACTIVE).bold(),
            title_suffix,
        ]),
        info_line,
    ]
}

fn build_remote_title_status_line(status: &RemoteDashboardStatus) -> Line<'static> {
    match status {
        RemoteDashboardStatus::Connecting { target } => Line::from(vec![
            "Remote: ".fg(styles::LABEL),
            "connecting".fg(Color::Yellow).bold(),
            "  |  Target: ".fg(styles::LABEL),
            target.to_string().fg(styles::VALUE_INFO),
        ]),
        RemoteDashboardStatus::Connected { target } => Line::from(vec![
            "Remote: ".fg(styles::LABEL),
            "live".fg(styles::VALUE_PRIMARY).bold(),
            "  |  Target: ".fg(styles::LABEL),
            target.to_string().fg(styles::VALUE_INFO),
        ]),
        RemoteDashboardStatus::Reconnecting {
            target,
            retry_delay,
            ..
        } => Line::from(vec![
            "Remote: ".fg(styles::LABEL),
            "reconnecting".fg(Color::Yellow).bold(),
            format!(" in {:.1}s", retry_delay.as_secs_f32()).fg(styles::LABEL),
            "  |  Target: ".fg(styles::LABEL),
            target.to_string().fg(styles::VALUE_INFO),
            "  |  Snapshot: ".fg(styles::LABEL),
            "last known".fg(Color::Yellow),
        ]),
    }
}

/// Render summary statistics
fn render_summary(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
    show_details: bool,
) {
    let metrics = &state.metrics;
    let system_stats = &state.system_stats;

    // Split summary box into three columns
    let [app_summary, cache_summary, transfer_summary] = split_summary_columns(area);

    render_app_summary_panel(
        f,
        app_summary,
        metrics,
        system_stats,
        state.buffer_pool(),
        show_details,
        attached_ui_stats,
    );
    render_cache_summary_panel(f, cache_summary, metrics);
    render_transfer_summary_panel(f, transfer_summary, state);
}

fn render_app_summary_panel(
    f: &mut Frame,
    area: Rect,
    metrics: &DashboardMetrics,
    system_stats: &crate::tui::SystemStats,
    buffer_pool: Option<&BufferPoolStats>,
    show_details: bool,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
) {
    fn cpu_color(usage: f32) -> Color {
        if usage > 80.0 {
            Color::Red
        } else if usage > 50.0 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    fn session_color(count: usize) -> Color {
        if count > 0 {
            styles::VALUE_PRIMARY
        } else {
            styles::VALUE_NEUTRAL
        }
    }

    fn buffer_color(in_use: usize, total: usize) -> Color {
        let percent = (in_use * 100).checked_div(total).unwrap_or_default();
        if percent > 80 {
            Color::Red
        } else if percent > 60 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    let block = bordered_block("App", styles::BORDER_NORMAL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.left().saturating_add(inner.width);
    let buffer = f.buffer_mut();
    let mut row = 0u16;

    let uptime = metrics.format_uptime();
    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Uptime: ",
        &uptime,
        Style::default().fg(styles::VALUE_PRIMARY),
    );

    let stateful = metrics.stateful_sessions.to_string();
    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Stateful Sessions: ",
        &stateful,
        Style::default().fg(session_color(metrics.stateful_sessions)),
    );

    if let Some(ui_stats) = attached_ui_stats {
        let proxy_cpu = percent_text(system_stats.cpu_usage);
        let ui_cpu = percent_text(ui_stats.cpu_usage);
        render_dual_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "CPU (proxy, UI): ",
            (
                &proxy_cpu,
                Style::default().fg(cpu_color(system_stats.cpu_usage)),
            ),
            (&ui_cpu, Style::default().fg(cpu_color(ui_stats.cpu_usage))),
        );

        let proxy_mem = format_bytes(system_stats.memory_bytes);
        let ui_mem = format_bytes(ui_stats.memory_bytes);
        render_dual_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Memory (proxy, UI): ",
            (&proxy_mem, Style::default().fg(styles::VALUE_INFO)),
            (&ui_mem, Style::default().fg(styles::VALUE_INFO)),
        );
    } else {
        let cpu = percent_text(system_stats.cpu_usage);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "CPU: ",
            &cpu,
            Style::default().fg(cpu_color(system_stats.cpu_usage)),
        );

        let mem = format_bytes(system_stats.memory_bytes);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Memory: ",
            &mem,
            Style::default().fg(styles::VALUE_INFO),
        );
    }

    let mut text = ArrayString::<48>::new();
    let _ = write!(&mut text, "{} requests", metrics.in_flight_requests);
    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Pending: ",
        &text,
        Style::default().fg(styles::VALUE_INFO),
    );

    if metrics.pipeline_batches > 0 {
        let avg_batch =
            counter_as_f64(metrics.pipeline_commands) / counter_as_f64(metrics.pipeline_batches);
        let mut text = ArrayString::<64>::new();
        let _ = write!(
            &mut text,
            "{} batches (avg {:.1} cmds)",
            metrics.pipeline_batches, avg_batch
        );
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Pipeline: ",
            &text,
            Style::default().fg(styles::VALUE_INFO),
        );
    }

    if metrics.pipeline_requests_queued > 0 {
        let mut text = ArrayString::<64>::new();
        let _ = write!(
            &mut text,
            "{} queued, {} completed",
            metrics.pipeline_requests_queued, metrics.pipeline_requests_completed
        );
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Mux Totals: ",
            &text,
            Style::default().fg(styles::VALUE_INFO),
        );
    }

    if show_details && let Some(pool) = buffer_pool {
        let usage_percent = if pool.total > 0 {
            size_as_f64(pool.in_use * 100) / size_as_f64(pool.total)
        } else {
            0.0
        };
        let mut text = ArrayString::<48>::new();
        let _ = write!(
            &mut text,
            "{}/{} ({usage_percent:.0}%)",
            pool.in_use, pool.total
        );
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Buffers: ",
            &text,
            Style::default().fg(buffer_color(pool.in_use, pool.total)),
        );
    }
}

fn render_cache_summary_panel(f: &mut Frame, area: Rect, metrics: &DashboardMetrics) {
    const fn entries_color(count: u64) -> Color {
        if count > 0 {
            styles::VALUE_INFO
        } else {
            styles::VALUE_NEUTRAL
        }
    }

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

    let block = bordered_block(cache_summary_title(metrics), styles::BORDER_NORMAL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.left().saturating_add(inner.width);
    let buffer = f.buffer_mut();
    let mut row = 0u16;
    let hit_rate = percent_text_f64(metrics.cache_hit_rate);

    if let Some(disk) = metrics.disk_cache.as_ref() {
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Hit Rate: ",
            &hit_rate,
            Style::default().fg(hit_rate_color(metrics.cache_hit_rate)),
        );
        let written = format_bytes(disk.bytes_written);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Disk Written: ",
            &written,
            Style::default().fg(non_zero_color(disk.bytes_written)),
        );
        let read = format_bytes(disk.bytes_read);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Disk Read: ",
            &read,
            Style::default().fg(non_zero_color(disk.bytes_read)),
        );
        let mut hits = ArrayString::<48>::new();
        let _ = write!(&mut hits, "{} ({:.1}%)", disk.disk_hits, disk.disk_hit_rate);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Disk Hits: ",
            &hits,
            Style::default().fg(non_zero_color(disk.disk_hits)),
        );
        let io_count = disk.write_ios.to_string();
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Write I/Os: ",
            &io_count,
            Style::default().fg(styles::VALUE_NEUTRAL),
        );
    } else {
        let entries = metrics.cache_entries.to_string();
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Entries: ",
            &entries,
            Style::default().fg(entries_color(metrics.cache_entries)),
        );
        let size = format_bytes(metrics.cache_size_bytes);
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Size: ",
            &size,
            Style::default().fg(styles::VALUE_NEUTRAL),
        );
        render_label_value_row(
            buffer,
            inner,
            &mut row,
            right,
            "Hit Rate: ",
            &hit_rate,
            Style::default().fg(hit_rate_color(metrics.cache_hit_rate)),
        );
    }
}

fn render_transfer_summary_panel(f: &mut Frame, area: Rect, state: &DashboardState) {
    let (client_to_backend, backend_to_client) =
        format_summary_throughput(state.latest_client_throughput());
    let total = format_bytes(state.metrics.total_bytes());

    let block = bordered_block("Data Transfer", styles::BORDER_NORMAL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.left().saturating_add(inner.width);
    let buffer = f.buffer_mut();
    let mut row = 0u16;

    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Client → Backend: ",
        &client_to_backend,
        Style::default().fg(styles::VALUE_SECONDARY),
    );
    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Backend → Client: ",
        &backend_to_client,
        Style::default()
            .fg(styles::VALUE_PRIMARY)
            .add_modifier(Modifier::BOLD),
    );
    render_label_value_row(
        buffer,
        inner,
        &mut row,
        right,
        "Total: ",
        &total,
        Style::default().fg(styles::VALUE_PRIMARY),
    );
}

fn render_label_value_row(
    buffer: &mut Buffer,
    area: Rect,
    row: &mut u16,
    right: u16,
    label: &str,
    value: &str,
    value_style: Style,
) {
    if *row >= area.height {
        return;
    }

    let y = area.top().saturating_add(*row);
    let mut x = area.left();
    write_part(
        buffer,
        &mut x,
        y,
        right,
        label,
        Style::default().fg(styles::LABEL),
    );
    write_part(buffer, &mut x, y, right, value, value_style);
    *row = row.saturating_add(1);
}

fn render_dual_value_row(
    buffer: &mut Buffer,
    area: Rect,
    row: &mut u16,
    right: u16,
    label: &str,
    value_a: (&str, Style),
    value_b: (&str, Style),
) {
    if *row >= area.height {
        return;
    }

    let y = area.top().saturating_add(*row);
    let mut x = area.left();
    write_part(
        buffer,
        &mut x,
        y,
        right,
        label,
        Style::default().fg(styles::LABEL),
    );
    write_part(buffer, &mut x, y, right, value_a.0, value_a.1);
    write_part(
        buffer,
        &mut x,
        y,
        right,
        " / ",
        Style::default().fg(styles::LABEL),
    );
    write_part(buffer, &mut x, y, right, value_b.0, value_b.1);
    *row = row.saturating_add(1);
}

fn percent_text(value: f32) -> ArrayString<16> {
    let mut text = ArrayString::<16>::new();
    let _ = write!(&mut text, "{value:.1}%");
    text
}

fn percent_text_f64(value: f64) -> ArrayString<16> {
    let mut text = ArrayString::<16>::new();
    let _ = write!(&mut text, "{value:.1}%");
    text
}

/// Render backend server visualization
fn render_backends(f: &mut Frame, area: Rect, state: &DashboardState, show_details: bool) {
    // Split into three columns: backend list, data flow chart, and top users
    let [backend_list, data_flow, user_stats] = split_backend_columns(area);

    render_backend_list(f, backend_list, state, show_details);
    render_data_flow(f, data_flow, state);
    render_user_stats(f, user_stats, &state.top_users);
}

// ============================================================================
// Summary Panel Builders (Pure Functions)
// ============================================================================

#[cfg(test)]
fn build_app_summary_lines(
    metrics: &DashboardMetrics,
    system_stats: &crate::tui::SystemStats,
    buffer_pool: Option<&BufferPoolStats>,
    show_details: bool,
    attached_ui_stats: Option<&crate::tui::SystemStats>,
) -> Vec<Line<'static>> {
    fn cpu_color(usage: f32) -> Color {
        if usage > 80.0 {
            Color::Red
        } else if usage > 50.0 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    fn session_color(count: usize) -> Color {
        if count > 0 {
            styles::VALUE_PRIMARY
        } else {
            styles::VALUE_NEUTRAL
        }
    }

    fn buffer_color(in_use: usize, total: usize) -> Color {
        let percent = (in_use * 100).checked_div(total).unwrap_or_default();
        if percent > 80 {
            Color::Red
        } else if percent > 60 {
            Color::Yellow
        } else {
            styles::VALUE_INFO
        }
    }

    let mut lines = vec![
        Line::from(vec![
            "Uptime: ".fg(styles::LABEL),
            metrics.format_uptime().fg(styles::VALUE_PRIMARY),
        ]),
        Line::from(vec![
            "Stateful Sessions: ".fg(styles::LABEL),
            format!("{}", metrics.stateful_sessions).fg(session_color(metrics.stateful_sessions)),
        ]),
    ];

    if let Some(ui_stats) = attached_ui_stats {
        lines.extend([
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
        ]);
    } else {
        lines.extend([
            Line::from(vec![
                "CPU: ".fg(styles::LABEL),
                format!("{:.1}%", system_stats.cpu_usage).fg(cpu_color(system_stats.cpu_usage)),
            ]),
            Line::from(vec![
                "Memory: ".fg(styles::LABEL),
                format_bytes(system_stats.memory_bytes).fg(styles::VALUE_INFO),
            ]),
        ]);
    }

    lines.push(Line::from(vec![
        "Pending: ".fg(styles::LABEL),
        format!("{} requests", metrics.in_flight_requests).fg(styles::VALUE_INFO),
    ]));

    if metrics.pipeline_batches > 0 {
        let avg_batch =
            counter_as_f64(metrics.pipeline_commands) / counter_as_f64(metrics.pipeline_batches);
        lines.push(Line::from(vec![
            "Pipeline: ".fg(styles::LABEL),
            format!(
                "{} batches (avg {:.1} cmds)",
                metrics.pipeline_batches, avg_batch
            )
            .fg(styles::VALUE_INFO),
        ]));
    }

    if metrics.pipeline_requests_queued > 0 {
        lines.push(Line::from(vec![
            "Mux Totals: ".fg(styles::LABEL),
            format!(
                "{} queued, {} completed",
                metrics.pipeline_requests_queued, metrics.pipeline_requests_completed
            )
            .fg(styles::VALUE_INFO),
        ]));
    }

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

    lines
}

fn cache_summary_title(metrics: &DashboardMetrics) -> &'static str {
    if metrics.disk_cache.is_some() {
        "Cache (Hybrid)"
    } else {
        "Cache"
    }
}

// ============================================================================
// Backend List Rendering Helpers
// ============================================================================

/// Render list of backend servers with their stats
fn render_backend_list(f: &mut Frame, area: Rect, state: &DashboardState, show_details: bool) {
    let block = bordered_block("Backend Servers", styles::BORDER_NORMAL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.left().saturating_add(inner.width);
    let buffer = f.buffer_mut();
    let mut row = 0u16;

    for (i, backend) in state.backend_views.iter().enumerate() {
        if row >= inner.height {
            break;
        }

        let backend_stats = &backend.stats;
        let server = &backend.server;
        let y = inner.top().saturating_add(row);
        let (health_icon, health_color) = health_indicator(backend.health_status);
        let error_rate = backend_stats.error_rate_percent();
        let error_text = format_error_rate(error_rate);
        let address = format!("{}:{}", server.host, server.port.get());
        let share_text = state
            .backend_traffic_share(i)
            .map(|share| format!(" ({share:.1}% share)"));
        let cmd_per_sec = backend
            .latest_throughput()
            .and_then(super::app::ThroughputPoint::commands_per_sec)
            .map_or_else(
                || text::DEFAULT_CMD_RATE.to_string(),
                |cps| format!("{:.0}", cps.get()),
            );
        let ttfb = backend_stats
            .average_ttfb_ms()
            .map_or_else(|| "N/A".to_string(), |ms| format!("{ms:.1}ms"));
        let sent = format_bytes(backend_stats.bytes_sent.as_u64());
        let received = format_bytes(backend_stats.bytes_received.as_u64());
        let avg_size = backend_stats
            .average_article_size()
            .map_or_else(|| "N/A".to_string(), format_bytes);
        let error_counts = format!(
            "4xx:{} 5xx:{}",
            backend_stats.errors_4xx.get(),
            backend_stats.errors_5xx.get()
        );
        let failures = backend_stats.connection_failures.get().to_string();
        let used_max = format!(
            "{}/{}",
            backend.active_connections,
            server.max_connections.get()
        );
        let article_count = backend_stats.article_count.get().to_string();

        let mut x = inner.left();
        write_part(
            buffer,
            &mut x,
            y,
            right,
            health_icon,
            Style::default()
                .fg(health_color)
                .add_modifier(Modifier::BOLD),
        );
        write_part(buffer, &mut x, y, right, " ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            server.name.as_str(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &error_text,
            Style::default().fg(error_rate_color(error_rate)),
        );
        row = row.saturating_add(1);

        if row >= inner.height {
            break;
        }
        let y = inner.top().saturating_add(row);
        let mut x = inner.left();
        write_part(buffer, &mut x, y, right, "  ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &address,
            Style::default().fg(styles::LABEL),
        );
        if let Some(ref share_text) = share_text {
            write_part(
                buffer,
                &mut x,
                y,
                right,
                share_text,
                Style::default().fg(Color::Cyan),
            );
        }
        row = row.saturating_add(1);

        if row >= inner.height {
            break;
        }
        let y = inner.top().saturating_add(row);
        let mut x = inner.left();
        write_part(
            buffer,
            &mut x,
            y,
            right,
            "  Used/Max: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &used_max,
            Style::default().fg(styles::VALUE_SECONDARY),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            " | Cmd/s: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &cmd_per_sec,
            Style::default().fg(styles::VALUE_INFO),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            " | TTFB: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &ttfb,
            Style::default().fg(styles::VALUE_INFO),
        );
        row = row.saturating_add(1);

        if row >= inner.height {
            break;
        }
        let y = inner.top().saturating_add(row);
        let mut x = inner.left();
        write_part(buffer, &mut x, y, right, "  ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            text::ARROW_UP,
            Style::default().fg(styles::VALUE_PRIMARY),
        );
        write_part(buffer, &mut x, y, right, " ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &sent,
            Style::default().fg(styles::VALUE_PRIMARY),
        );
        write_part(buffer, &mut x, y, right, "  ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            text::ARROW_DOWN,
            Style::default().fg(styles::VALUE_NEUTRAL),
        );
        write_part(buffer, &mut x, y, right, " ", Style::default());
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &received,
            Style::default().fg(styles::VALUE_NEUTRAL),
        );
        row = row.saturating_add(1);

        if row >= inner.height {
            break;
        }
        let y = inner.top().saturating_add(row);
        let mut x = inner.left();
        write_part(
            buffer,
            &mut x,
            y,
            right,
            "  Avg Article: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &avg_size,
            Style::default().fg(styles::VALUE_INFO),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            " | Articles: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &article_count,
            Style::default().fg(styles::VALUE_NEUTRAL),
        );
        row = row.saturating_add(1);

        if row >= inner.height {
            break;
        }
        let y = inner.top().saturating_add(row);
        let mut x = inner.left();
        write_part(
            buffer,
            &mut x,
            y,
            right,
            "  Errors: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &error_counts,
            Style::default().fg(error_count_color(!backend_stats.errors.is_zero())),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            " | Conn Fails: ",
            Style::default().fg(styles::LABEL),
        );
        write_part(
            buffer,
            &mut x,
            y,
            right,
            &failures,
            Style::default().fg(connection_failure_color(
                backend_stats.connection_failures.get(),
            )),
        );
        row = row.saturating_add(1);

        if show_details && row < inner.height {
            let y = inner.top().saturating_add(row);
            let mut x = inner.left();
            let pending = state.backend_pending_count(i);
            let stateful = state.backend_stateful_count(i);
            let detail_text = backend_details_text(pending, stateful);
            write_part(
                buffer,
                &mut x,
                y,
                right,
                &detail_text,
                Style::default().fg(styles::LABEL),
            );
            row = row.saturating_add(1);
        }

        if i + 1 != state.backend_views.len() && row < inner.height {
            row = row.saturating_add(1);
        }
    }
}

fn write_part(buffer: &mut Buffer, x: &mut u16, y: u16, right: u16, text: &str, style: Style) {
    if *x >= right || text.is_empty() {
        return;
    }

    let remaining = right.saturating_sub(*x) as usize;
    let (next_x, _) = buffer.set_stringn(*x, y, text, remaining, style);
    *x = next_x;
}

fn backend_details_text(pending: usize, stateful: usize) -> String {
    let mut text = String::new();

    if stateful > 0 {
        text.push_str(&format!("  Stateful: {stateful} | "));
    }

    text.push_str(&format!("  Pending: {pending}"));

    text
}

#[cfg(test)]
fn backend_row_texts(state: &DashboardState, show_details: bool) -> Vec<String> {
    let mut rows = Vec::new();

    for (i, backend) in state.backend_views.iter().enumerate() {
        let backend_stats = &backend.stats;
        let server = &backend.server;
        let error_rate = backend_stats.error_rate_percent();
        let cmd_per_sec = backend
            .latest_throughput()
            .and_then(super::app::ThroughputPoint::commands_per_sec)
            .map_or_else(
                || text::DEFAULT_CMD_RATE.to_string(),
                |cps| format!("{:.0}", cps.get()),
            );
        let ttfb = backend_stats
            .average_ttfb_ms()
            .map_or_else(|| "N/A".to_string(), |ms| format!("{ms:.1}ms"));
        let avg_size = backend_stats
            .average_article_size()
            .map_or_else(|| "N/A".to_string(), format_bytes);

        rows.push(format!(
            "{} {}{}",
            health_indicator(backend.health_status).0,
            server.name,
            format_error_rate(error_rate)
        ));
        rows.push(match state.backend_traffic_share(i) {
            Some(share) => format!(
                "  {}:{} ({share:.1}% share)",
                server.host,
                server.port.get()
            ),
            None => format!("  {}:{}", server.host, server.port.get()),
        });
        rows.push(format!(
            "  Used/Max: {}/{} | Cmd/s: {} | TTFB: {}",
            backend.active_connections,
            server.max_connections.get(),
            cmd_per_sec,
            ttfb
        ));
        rows.push(format!(
            "  {} {}  {} {}",
            text::ARROW_UP,
            format_bytes(backend_stats.bytes_sent.as_u64()),
            text::ARROW_DOWN,
            format_bytes(backend_stats.bytes_received.as_u64())
        ));
        rows.push(format!(
            "  Avg Article: {} | Articles: {}",
            avg_size,
            backend_stats.article_count.get()
        ));
        rows.push(format!(
            "  Errors: 4xx:{} 5xx:{} | Conn Fails: {}",
            backend_stats.errors_4xx.get(),
            backend_stats.errors_5xx.get(),
            backend_stats.connection_failures.get()
        ));

        if show_details {
            rows.push(backend_details_text(
                state.backend_pending_count(i),
                state.backend_stateful_count(i),
            ));
        }

        if i + 1 != state.backend_views.len() {
            rows.push(String::new());
        }
    }

    rows
}

/// Render data flow visualization as line graphs
fn render_data_flow(f: &mut Frame, area: Rect, state: &DashboardState) {
    // Build chart data in single pass (no nested loops)
    let (chart_data, max_throughput) = build_chart_data(&state.backend_views);

    if max_throughput <= 0.0 {
        let placeholder = Line::from(Span::styled(
            "No throughput samples yet",
            Style::default().fg(styles::LABEL),
        ));
        render_block_lines(
            f,
            area,
            bordered_block(chart::TITLE, styles::BORDER_NORMAL),
            std::slice::from_ref(&placeholder),
            Alignment::Center,
        );
        return;
    }

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
    let footer = footer_help_line();
    render_block_lines(
        f,
        area,
        untitled_bordered_block(styles::LABEL),
        std::slice::from_ref(&footer),
        Alignment::Center,
    );
}

fn footer_help_line() -> Line<'static> {
    Line::from(vec![
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
    ])
}

/// Render recent log messages
fn render_logs(f: &mut Frame, area: Rect, log_lines: &[String], show_details: bool) {
    let visible_lines = area.height.saturating_sub(2) as usize;
    let fetch_count = if show_details {
        visible_lines * 3
    } else {
        visible_lines
    };

    if show_details {
        let paragraph = Paragraph::new(recent_log_text_lines(log_lines, fetch_count))
            .style(Style::default().fg(Color::Gray))
            .block(bordered_block(" Recent Logs ", styles::BORDER_ACTIVE))
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
        return;
    }

    let block = bordered_block(" Recent Logs ", styles::BORDER_ACTIVE);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    for (row, line) in recent_log_lines(log_lines, fetch_count)
        .iter()
        .take(inner.height as usize)
        .enumerate()
    {
        let rendered = Line::from(Span::styled(
            line.as_str(),
            Style::default().fg(Color::Gray),
        ));
        f.buffer_mut().set_line(
            inner.left(),
            inner.top().saturating_add(row as u16),
            &rendered,
            inner.width,
        );
    }
}

fn render_block_lines(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    lines: &[Line<'_>],
    alignment: Alignment,
) {
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let buffer = f.buffer_mut();
    for (row, line) in lines.iter().take(inner.height as usize).enumerate() {
        render_line(buffer, inner, row as u16, line, alignment);
    }
}

fn render_line(buffer: &mut Buffer, area: Rect, row: u16, line: &Line<'_>, alignment: Alignment) {
    let line_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let offset = match alignment {
        Alignment::Center => area.width.saturating_sub(line_width) / 2,
        Alignment::Right => area.width.saturating_sub(line_width),
        Alignment::Left => 0,
    };
    let x = area.left().saturating_add(offset);
    let width = area.width.saturating_sub(offset);
    buffer.set_line(x, area.top().saturating_add(row), line, width);
}

fn recent_log_lines(lines: &[String], count: usize) -> &[String] {
    let start = lines.len().saturating_sub(count);
    &lines[start..]
}

fn recent_log_text_lines(lines: &[String], count: usize) -> Vec<Line<'_>> {
    recent_log_lines(lines, count)
        .iter()
        .map(|line| Line::from(line.as_str()))
        .collect()
}

/// Render per-user statistics panel
fn render_user_stats(f: &mut Frame, area: Rect, top_users: &[DashboardUserStats]) {
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
    fn user_stat_lines(user: &DashboardUserStats, sparkline: String) -> Vec<Line<'static>> {
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
        .iter()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{CommandCount, ErrorCount};
    use crate::tui::app::ViewMode;
    use crate::tui::dashboard::{BufferPoolStats, DashboardMetrics};
    use ratatui::layout::Rect;

    fn user_stats(name: &str, total_bytes: u64) -> DashboardUserStats {
        DashboardUserStats {
            username: name.to_string(),
            active_connections: 0,
            total_connections: crate::types::TotalConnections::new(0),
            bytes_sent: crate::types::BytesSent::new(total_bytes),
            bytes_received: crate::types::BytesReceived::ZERO,
            bytes_sent_per_sec: crate::types::BytesPerSecondRate::new(0),
            bytes_received_per_sec: crate::types::BytesPerSecondRate::new(0),
            total_commands: CommandCount::new(0),
            errors: ErrorCount::new(0),
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
    fn recent_log_text_lines_preserve_visible_content_without_joining() {
        let lines = vec!["one".to_string(), "two".to_string(), "three".to_string()];

        let rendered = recent_log_text_lines(&lines, 2)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["two".to_string(), "three".to_string()]);
    }

    #[test]
    fn footer_help_line_preserves_help_text() {
        assert_eq!(
            footer_help_line().to_string(),
            "Press q or Esc to exit  |  L to toggle logs  |  d for details  |  Ctrl+C to shutdown"
        );
    }

    #[test]
    fn attached_render_cache_prebuilds_remote_title_and_backend_strings() {
        let state = DashboardState {
            metrics: DashboardMetrics {
                active_connections: 3,
                total_connections: 5,
                ..DashboardMetrics::default()
            },
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["line".to_string()],
            buffer_pool: Some(BufferPoolStats {
                available: 3,
                in_use: 1,
                total: 4,
            }),
        };

        let cache = build_attached_render_cache(
            &state,
            &RemoteDashboardStatus::Connected {
                target: "127.0.0.1:8120".parse().unwrap(),
            },
        );

        assert!(
            cache.title_lines[0]
                .to_string()
                .contains("Attached Dashboard (live)")
        );
        assert_eq!(cache.title_lines.len(), 2);
    }

    #[test]
    fn render_user_stats_uses_pre_sorted_users() {
        let users = [
            user_stats("bob", 30),
            user_stats("carol", 20),
            user_stats("alice", 10),
        ];
        let rendered = users
            .iter()
            .map(|user| {
                user_stat_lines_for_test(user)
                    .into_iter()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert!(rendered[0].contains("bob"));
        assert!(rendered[1].contains("carol"));
        assert!(rendered[2].contains("alice"));
    }

    #[test]
    fn app_summary_lines_switch_between_local_and_remote_stats() {
        let metrics = DashboardMetrics::default();
        let system_stats = crate::tui::SystemStats::default();
        let buffer_pool = BufferPoolStats {
            available: 3,
            in_use: 2,
            total: 5,
        };

        let local_lines =
            build_app_summary_lines(&metrics, &system_stats, Some(&buffer_pool), true, None)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>();
        let remote_lines = build_app_summary_lines(
            &metrics,
            &system_stats,
            Some(&buffer_pool),
            true,
            Some(&crate::tui::SystemStats {
                cpu_usage: 12.5,
                memory_bytes: 42,
                ..Default::default()
            }),
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

        assert!(local_lines.iter().any(|line| line.contains("CPU:")));
        assert!(local_lines.iter().any(|line| line.contains("Memory:")));
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("CPU (proxy, UI):"))
        );
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("Memory (proxy, UI):"))
        );
        assert!(
            !remote_lines
                .iter()
                .any(|line| line.contains("CPU:") && !line.contains("proxy"))
        );
    }

    #[test]
    fn dashboard_layout_helpers_distinguish_modes_and_height() {
        assert!(should_show_dashboard_logs(layout::MIN_HEIGHT_FOR_LOGS));
        assert!(!should_show_dashboard_logs(layout::MIN_HEIGHT_FOR_LOGS - 1));

        let fullscreen =
            dashboard_fullscreen_chunks(ViewMode::LogFullscreen, Rect::new(0, 0, 80, 24))
                .expect("fullscreen layout");
        assert_eq!(fullscreen[0], Rect::new(1, 1, 78, 3));
        assert_eq!(fullscreen[1], Rect::new(1, 4, 78, 16));
        assert_eq!(fullscreen[2], Rect::new(1, 20, 78, 3));
        assert!(dashboard_fullscreen_chunks(ViewMode::Normal, Rect::new(0, 0, 80, 24)).is_none());

        let with_logs = dashboard_main_chunks(Rect::new(0, 0, 80, 40), true);
        assert_eq!(with_logs.len(), 5);
        let DashboardMainChunks::WithLogs([title, summary, backends, logs, footer]) = with_logs
        else {
            panic!("expected layout with logs");
        };
        assert_eq!(title, Rect::new(1, 1, 78, 3));
        assert_eq!(summary, Rect::new(1, 4, 78, 6));
        assert_eq!(backends, Rect::new(1, 10, 78, 16));
        assert_eq!(logs, Rect::new(1, 26, 78, 10));
        assert_eq!(footer, Rect::new(1, 36, 78, 3));

        let without_logs = dashboard_main_chunks(Rect::new(0, 0, 80, 24), false);
        assert_eq!(without_logs.len(), 4);
        let DashboardMainChunks::WithoutLogs([title, summary, backends, footer]) = without_logs
        else {
            panic!("expected layout without logs");
        };
        assert_eq!(title, Rect::new(1, 1, 78, 3));
        assert_eq!(summary, Rect::new(1, 4, 78, 6));
        assert_eq!(backends, Rect::new(1, 10, 78, 10));
        assert_eq!(footer, Rect::new(1, 20, 78, 3));
    }

    #[test]
    fn manual_horizontal_layout_helpers_match_expected_proportions() {
        let [app_summary, cache_summary, transfer_summary] =
            split_summary_columns(Rect::new(10, 5, 100, 6));
        assert_eq!(app_summary, Rect::new(10, 5, 33, 6));
        assert_eq!(cache_summary, Rect::new(43, 5, 34, 6));
        assert_eq!(transfer_summary, Rect::new(77, 5, 33, 6));

        let [backend_list, data_flow, user_stats] = split_backend_columns(Rect::new(2, 3, 100, 8));
        assert_eq!(backend_list, Rect::new(2, 3, 35, 8));
        assert_eq!(data_flow, Rect::new(37, 3, 43, 8));
        assert_eq!(user_stats, Rect::new(80, 3, 22, 8));
    }

    #[test]
    fn title_lines_switch_between_local_and_remote_status() {
        let metrics = DashboardMetrics::default();
        let local_lines = build_title_lines(&metrics, None)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let remote_lines = build_title_lines(
            &metrics,
            Some(&RemoteDashboardStatus::Reconnecting {
                target: "127.0.0.1:8120".parse().unwrap(),
                retry_delay: std::time::Duration::from_secs(1),
                last_error: "connection dropped".to_string(),
            }),
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

        assert!(
            local_lines
                .iter()
                .any(|line| line.contains("Real-Time Metrics Dashboard"))
        );
        assert!(local_lines.iter().any(|line| line.contains("Uptime:")));
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("Attached Dashboard (reconnecting)"))
        );
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("Remote: reconnecting"))
        );
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("Snapshot: last known"))
        );
    }

    #[test]
    fn backend_details_line_includes_pending_and_stateful_counts() {
        let details = backend_details_text(4, 1);

        assert!(details.contains("Stateful: 1"));
        assert!(details.contains("Pending: 4"));
        assert!(!details.contains('%'));
    }

    #[test]
    fn backend_display_lines_insert_blank_lines_between_backends() {
        let state = DashboardState {
            metrics: DashboardMetrics::default(),
            backend_views: vec![test_backend_view("one"), test_backend_view("two")],
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: Vec::new(),
            buffer_pool: None,
        };

        let lines = backend_row_texts(&state, false);

        assert!(lines.iter().any(|line| line.contains("one")));
        assert!(lines.iter().any(|line| line.contains("two")));
        assert!(lines.iter().any(String::is_empty));
    }

    fn test_backend_view(name: &str) -> crate::tui::dashboard::BackendView {
        use crate::metrics::{BackendHealthStatus, BackendStats};
        use crate::tui::dashboard::{BackendDisplay, BackendView};
        use crate::types::{HostName, MaxConnections, Port, ServerName};

        BackendView {
            server: BackendDisplay {
                host: HostName::try_new("backend.example.com".to_string()).unwrap(),
                port: Port::try_new(119).unwrap(),
                name: ServerName::try_new(name.to_string()).unwrap(),
                max_connections: MaxConnections::try_new(10).unwrap(),
            },
            stats: BackendStats::default(),
            active_connections: 0,
            health_status: BackendHealthStatus::Healthy,
            pending_count: 0,
            load_ratio: None,
            stateful_count: 0,
            traffic_share: None,
            history: Vec::new(),
        }
    }

    fn user_stat_lines_for_test(user: &DashboardUserStats) -> Vec<Line<'static>> {
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

        vec![
            Line::from(vec![
                format_username(&user.username).fg(Color::Cyan),
                " ".into(),
                create_sparkline(user.total_bytes(), user.total_bytes()).fg(Color::Blue),
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
}
