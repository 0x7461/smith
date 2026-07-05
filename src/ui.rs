use chrono::{DateTime, Local};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::app::{App, PanelMode, View};
use crate::build::BuildJobStatus;
use crate::dep_graph::TreeNode;
use crate::package::Status;

// Catppuccin Macchiato palette
const GREEN: Color = Color::Rgb(166, 218, 149);
const RED: Color = Color::Rgb(237, 135, 150);
const YELLOW: Color = Color::Rgb(238, 212, 159);
const PEACH: Color = Color::Rgb(245, 169, 127);
const TEAL: Color = Color::Rgb(139, 213, 202);
const TEXT: Color = Color::Rgb(202, 211, 245);
const SURFACE0: Color = Color::Rgb(54, 58, 79);
const OVERLAY0: Color = Color::Rgb(110, 115, 141);
const BASE: Color = Color::Rgb(36, 39, 58);

fn format_local_time(unix_secs: u64) -> String {
    DateTime::from_timestamp(unix_secs as i64, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn elapsed_label(secs: u64) -> String {
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn status_color(status: &Status) -> Color {
    match status {
        Status::UpToDate => GREEN,
        Status::BuildOutdated => YELLOW,
        Status::ReadyToInstall => TEAL,
        Status::BuildFailed => RED,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let panel_height = match app.panel {
        PanelMode::None => 0,
        PanelMode::Detail => 10,
        PanelMode::BuildLog => 12,
        PanelMode::BumpLog => 12,
        PanelMode::GitMenu => 10,
        PanelMode::Help => 16,
    };

    let chunks = if panel_height > 0 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),           // header
                Constraint::Min(8),              // main
                Constraint::Length(panel_height), // panel
                Constraint::Length(1),            // status bar
            ])
            .split(f.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(8),   // main
                Constraint::Length(1), // status bar
            ])
            .split(f.area())
    };

    let visible = app.visible_packages();
    let visible_len = visible.len();
    let visible_indices: Vec<usize> = visible.iter().map(|(i, _)| *i).collect();
    drop(visible);

    draw_header(f, app, chunks[0], visible_len);
    match app.view {
        View::List => draw_package_list(f, &mut app.table_state, app.selected, &app.packages, &visible_indices, chunks[1]),
        View::Tree => draw_tree_view(f, app, chunks[1]),
    }

    if panel_height > 0 {
        match app.panel {
            PanelMode::Detail => draw_detail(f, app, chunks[2]),
            PanelMode::BuildLog => draw_build_log(f, app, chunks[2]),
            PanelMode::BumpLog => draw_bump_log(f, app, chunks[2]),
            PanelMode::GitMenu => draw_git_panel(f, app, chunks[2]),
            PanelMode::Help => draw_help_panel(f, chunks[2]),
            PanelMode::None => {}
        }
        draw_status_bar(f, app, chunks[3]);
    } else {
        draw_status_bar(f, app, chunks[chunks.len() - 1]);
    }

    // Confirmation modals — rendered last so they float above everything
    if let Some(ref warnings) = app.build_preflight {
        draw_preflight_modal(f, warnings);
    } else if let Some(ref op) = app.cancel_confirm {
        let msg = format!("Cancel {} in progress?  y = yes   Esc = no", op);
        draw_modal(f, &msg, RED);
    } else if app.quit_confirm {
        draw_modal(f, "Quit while operation is running?  y = yes   Esc = no", YELLOW);
    }
}

/// Multi-line pre-flight warning modal. Meaning lives in the text labels (⚠ + key hints),
/// not color, so it stays legible regardless of color perception.
fn draw_preflight_modal(f: &mut Frame, warnings: &[crate::build::PreflightWarning]) {
    let area = f.area();
    let cleanable = warnings.iter().any(|w| w.cleanable);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Build pre-flight",
        Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for w in warnings {
        lines.push(Line::from(format!("⚠ {}", w.message)));
    }
    lines.push(Line::from(""));
    let hint = if cleanable {
        "c = clean & build     b = build anyway     Esc = dismiss (do nothing)"
    } else {
        "b = build anyway     Esc = dismiss (do nothing)"
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(OVERLAY0))));

    let width = (area.width as f32 * 0.7) as u16;
    let width = width.clamp(40, area.width.saturating_sub(4));
    let inner = width.saturating_sub(2).max(1) as usize;
    // Account for word-wrapping when estimating height.
    let rows: usize = lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(inner))
        .sum();
    let height = (rows as u16 + 2).min(area.height.saturating_sub(2));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(YELLOW))
                .title(" warning "),
        )
        .style(Style::default().fg(TEXT))
        .wrap(Wrap { trim: true });
    f.render_widget(para, popup);
}

fn draw_modal(f: &mut Frame, text: &str, border_color: Color) {
    let width = (text.len() as u16 + 4).min(f.area().width.saturating_sub(4));
    let area = f.area();
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(3) / 2;
    let popup = Rect { x, y, width, height: 3 };
    f.render_widget(Clear, popup);
    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(border_color)))
        .style(Style::default().fg(TEXT));
    f.render_widget(para, popup);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect, visible_len: usize) {
    let mut spans = vec![
        Span::styled(" VPM ", Style::default().fg(BASE).bg(TEAL).add_modifier(Modifier::BOLD)),
        Span::styled(" Void Package Manager", Style::default().fg(TEXT)),
    ];

    if let Some(ref gs) = app.git_status {
        // Branch + ahead/behind
        spans.push(Span::styled("  on ", Style::default().fg(OVERLAY0)));
        spans.push(Span::styled(gs.branch.clone(), Style::default().fg(TEXT)));
        if gs.ahead > 0 {
            spans.push(Span::styled(
                format!(" ({} ahead)", gs.ahead),
                Style::default().fg(PEACH),
            ));
        }
        if gs.behind > 0 {
            spans.push(Span::styled(
                format!(" ({} behind)", gs.behind),
                Style::default().fg(YELLOW),
            ));
        }

        // Void fetch status
        if let Some(fetch_time) = gs.last_fetch {
            if let Ok(elapsed) = fetch_time.elapsed() {
                let secs = elapsed.as_secs();
                let stale = secs > 3 * 86400;
                let label = elapsed_label(secs);
                let color = if stale { YELLOW } else { OVERLAY0 };
                spans.push(Span::styled(
                    format!("  |  void: {}", label),
                    Style::default().fg(color),
                ));
            }
        }
    }

    // Pkg upstream check status
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last_label = match app.pkg_last_checked {
            Some(ts) => format_local_time(ts),
            None => "never".to_string(),
        };
        let unchecked = app.unchecked_count();
        let pkg_str = if unchecked > 0 {
            format!("  |  pkgs: {} unchecked (last: {})", unchecked, last_label)
        } else {
            format!("  |  pkgs: last {}", last_label)
        };
        let stale = match app.pkg_last_checked {
            Some(ts) => now.saturating_sub(ts) > 3 * 86400,
            None => true,
        };
        let color = if stale || unchecked > 0 { YELLOW } else { OVERLAY0 };
        spans.push(Span::styled(pkg_str, Style::default().fg(color)));
    }

    // Filter indicator
    if app.filter_active {
        spans.push(Span::styled("  / ", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(&app.filter, Style::default().fg(TEXT)));
        spans.push(Span::styled("█", Style::default().fg(TEXT)));
        spans.push(Span::styled(
            format!("  {}/{}", visible_len, app.packages.len()),
            Style::default().fg(OVERLAY0),
        ));
    } else if !app.filter.is_empty() {
        spans.push(Span::styled("  filter: ", Style::default().fg(OVERLAY0)));
        spans.push(Span::styled(&app.filter, Style::default().fg(TEXT)));
        spans.push(Span::styled(
            format!("  {}/{}", visible_len, app.packages.len()),
            Style::default().fg(OVERLAY0),
        ));
    }

    let header = Paragraph::new(Line::from(spans));
    f.render_widget(header, area);
}

fn draw_package_list(
    f: &mut Frame,
    table_state: &mut TableState,
    selected: usize,
    packages: &[crate::package::PackageState],
    visible_indices: &[usize],
    area: Rect,
) {
    let header_cells = ["Package", "Template", "Installed", "Latest", "Status"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(TEAL).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = visible_indices
        .iter()
        .enumerate()
        .map(|(i, &orig_idx)| {
            let ps = &packages[orig_idx];
            let installed_display = ps
                .installed
                .as_ref()
                .map(|v| {
                    v.rfind('-')
                        .map(|idx| v[idx + 1..].to_string())
                        .unwrap_or_else(|| v.clone())
                })
                .unwrap_or_else(|| "-".to_string());

            let latest_display = ps.latest.as_deref().unwrap_or("-").to_string();

            let style = if i == selected {
                Style::default().bg(SURFACE0).fg(TEXT)
            } else {
                Style::default().fg(TEXT)
            };

            // Build status label with optional badges
            let mut status_label = if ps.uncommitted {
                "UNCOMMITTED".to_string()
            } else {
                ps.status.label().to_string()
            };
            if !ps.soname_mismatches.is_empty() {
                status_label.push_str(" !so");
            }
            // Upstream update available — orthogonal to the lifecycle status above.
            if !ps.uncommitted && ps.upstream_newer() {
                status_label.push_str(" ↑");
            }

            let status_fg = if ps.uncommitted {
                OVERLAY0
            } else if !ps.soname_mismatches.is_empty() {
                if ps.status == Status::UpToDate { PEACH } else { status_color(&ps.status) }
            } else {
                status_color(&ps.status)
            };

            let final_status_style = if i == selected {
                Style::default()
                    .bg(SURFACE0)
                    .fg(status_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(status_fg)
                    .add_modifier(Modifier::BOLD)
            };

            Row::new(vec![
                Cell::from(ps.package.name.clone()).style(style),
                Cell::from(ps.package.version.clone()).style(style),
                Cell::from(installed_display).style(style),
                Cell::from(latest_display).style(style),
                Cell::from(status_label).style(final_status_style),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .row_highlight_style(Style::default()) // highlighting done per-cell above
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title_style(Style::default().fg(TEAL)),
    );

    table_state.select(Some(selected));
    f.render_stateful_widget(table, area, table_state);
}

fn draw_tree_view(f: &mut Frame, app: &App, area: Rect) {
    let selected = match app.selected_package() {
        Some(p) => p,
        None => return,
    };

    let name = &selected.package.name;
    let tree = app.dep_graph.reverse_dep_tree(name);

    let mut lines = vec![Line::from(vec![
        Span::styled(
            name.clone(),
            Style::default()
                .fg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" (v{})", selected.package.version),
            Style::default().fg(OVERLAY0),
        ),
    ])];

    render_tree_lines(&tree, "", true, &mut lines);

    if tree.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no reverse dependencies)",
            Style::default().fg(OVERLAY0),
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title(Span::styled(
                " Dependency Tree ",
                Style::default().fg(TEAL),
            )),
    );
    f.render_widget(para, area);
}

fn render_tree_lines(nodes: &[TreeNode], prefix: &str, is_root: bool, lines: &mut Vec<Line<'static>>) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        let connector = if is_root {
            if is_last { "└── " } else { "├── " }
        } else if is_last {
            "└── "
        } else {
            "├── "
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}{}", prefix, connector), Style::default().fg(OVERLAY0)),
            Span::styled(node.name.clone(), Style::default().fg(TEXT)),
        ]));

        let child_prefix = format!(
            "{}{}",
            prefix,
            if is_last { "    " } else { "│   " }
        );
        render_tree_lines(&node.children, &child_prefix, false, lines);
    }
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let selected = match app.selected_package() {
        Some(p) => p,
        None => return,
    };

    let pkg = &selected.package;
    let template_ver = format!("{}_{}", pkg.version, pkg.revision);

    let rev_deps = app
        .dep_graph
        .reverse
        .get(&pkg.name)
        .map(|d| {
            let mut v: Vec<&String> = d.iter().collect();
            v.sort();
            v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        })
        .unwrap_or_default();

    let fwd_deps = app
        .dep_graph
        .forward
        .get(&pkg.name)
        .map(|d| {
            let mut v: Vec<&String> = d.iter().collect();
            v.sort();
            v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        })
        .unwrap_or_default();

    // Version pipeline: upstream → template → build → system
    let upstream_ver = selected.latest.as_deref().unwrap_or("?");
    let build_ver = selected.built.as_deref().unwrap_or("—");
    let system_ver = selected.installed.as_ref()
        .map(|v| v.rfind('-').map(|i| v[i+1..].to_string()).unwrap_or_else(|| v.clone()))
        .unwrap_or_else(|| "—".to_string());

    let upstream_color = match selected.latest.as_deref() {
        None => OVERLAY0,
        Some(v) if crate::package::version_newer_pub(v, &pkg.version) => PEACH,
        _ => GREEN,
    };
    let build_color = if selected.built.as_deref().map(|b| b == template_ver).unwrap_or(false) {
        GREEN
    } else if selected.built.is_none() {
        OVERLAY0
    } else {
        YELLOW
    };
    let system_color = match &selected.installed {
        None => OVERLAY0,
        Some(_) => match &selected.built {
            Some(b) if system_ver == *b => GREEN,
            _ => YELLOW,
        },
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(pkg.short_desc.clone(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  Homepage:  ", Style::default().fg(OVERLAY0)),
            Span::styled(pkg.homepage.clone(), Style::default().fg(TEAL)),
        ]),
        Line::from(vec![
            Span::styled("  Status:    ", Style::default().fg(OVERLAY0)),
            Span::styled(
                selected.status.label(),
                Style::default().fg(status_color(&selected.status)).add_modifier(Modifier::BOLD),
            ),
            if selected.uncommitted {
                Span::styled("  UNCOMMITTED — git add && git commit to track", Style::default().fg(OVERLAY0))
            } else {
                Span::styled(
                    format!("  — {}", selected.action_hint()),
                    Style::default().fg(OVERLAY0),
                )
            },
        ]),
        Line::from(vec![
            Span::styled("  upstream:  ", Style::default().fg(OVERLAY0)),
            Span::styled(upstream_ver.to_string(), Style::default().fg(upstream_color)),
            Span::styled("  template:  ", Style::default().fg(OVERLAY0)),
            Span::styled(template_ver.clone(), Style::default().fg(TEXT)),
            Span::styled("  build:  ", Style::default().fg(OVERLAY0)),
            Span::styled(build_ver.to_string(), Style::default().fg(build_color)),
            Span::styled("  system:  ", Style::default().fg(OVERLAY0)),
            Span::styled(system_ver.clone(), Style::default().fg(system_color)),
        ]),
        Line::from(vec![
            Span::styled("  Depends on: ", Style::default().fg(OVERLAY0)),
            Span::styled(
                if fwd_deps.is_empty() { "(none)".to_string() } else { fwd_deps },
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Depended by: ", Style::default().fg(OVERLAY0)),
            Span::styled(
                if rev_deps.is_empty() { "(none)".to_string() } else { rev_deps },
                Style::default().fg(TEXT),
            ),
        ]),
    ];

    // Shared libs line
    if !selected.shlibs.is_empty() {
        let mut shlib_spans = vec![
            Span::styled("  Shared libs: ", Style::default().fg(OVERLAY0)),
        ];
        for entry in &selected.shlibs {
            let mismatch = selected
                .soname_mismatches
                .iter()
                .find(|m| m.registered == entry.soname);
            if let Some(m) = mismatch {
                shlib_spans.push(Span::styled(
                    format!("{} (installed: {} — MISMATCH) ", entry.soname, m.installed),
                    Style::default().fg(PEACH),
                ));
            } else {
                shlib_spans.push(Span::styled(
                    format!("{} (OK) ", entry.soname),
                    Style::default().fg(GREEN),
                ));
            }
        }
        lines.push(Line::from(shlib_spans));
    }

    // Build log path
    if let Some(ref log_path) = selected.build_log {
        let log_color = if selected.status == Status::BuildFailed { RED } else { TEXT };
        lines.push(Line::from(vec![
            Span::styled("  Build log: ", Style::default().fg(OVERLAY0)),
            Span::styled(log_path.clone(), Style::default().fg(log_color)),
        ]));
    }

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(SURFACE0))
                .title(Span::styled(
                    format!(" {} ", pkg.name),
                    Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_build_log(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Queue status line: [OK] pkg  [BUILD] pkg  [WAIT] pkg
    let mut queue_spans: Vec<Span<'static>> = vec![Span::styled("  ", Style::default())];
    for job in &app.build_queue.jobs {
        let (badge, color) = match job.status {
            BuildJobStatus::Success => ("[OK]", GREEN),
            BuildJobStatus::Building => ("[BUILD]", TEAL),
            BuildJobStatus::Failed => ("[FAIL]", RED),
            BuildJobStatus::Pending => ("[WAIT]", OVERLAY0),
        };
        queue_spans.push(Span::styled(
            format!("{} ", badge),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        queue_spans.push(Span::styled(
            format!("{}  ", job.name),
            Style::default().fg(TEXT),
        ));
    }
    lines.push(Line::from(queue_spans));

    // Scrolling build output — show last N lines that fit, offset by scroll
    let available = area.height.saturating_sub(3) as usize; // borders + queue line
    let output = &app.build_queue.current_output;
    let tail = output.len().saturating_sub(app.build_log_scroll);
    let start = tail.saturating_sub(available);
    for line_text in &output[start..tail] {
        let color = if line_text.starts_with("ERR:") { RED } else { TEXT };
        lines.push(Line::from(Span::styled(
            format!("  {}", line_text),
            Style::default().fg(color),
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title(Span::styled(
                " Build Log ",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(para, area);
}

fn draw_bump_log(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Read log file content
    let output: Vec<String> = if let Some(ref path) = app.bump_log_path {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    } else {
        vec!["Waiting for bump to start...".to_string()]
    };

    let available = area.height.saturating_sub(2) as usize; // borders
    let tail = output.len().saturating_sub(app.bump_log_scroll);
    let start = tail.saturating_sub(available);
    for line_text in &output[start..tail] {
        let color = if line_text.starts_with("=> FAILED") { RED } else { TEXT };
        lines.push(Line::from(Span::styled(
            format!("  {}", line_text),
            Style::default().fg(color),
        )));
    }

    let title = if app.template_bumping { " Bump Log (in progress) " } else { " Bump Log " };
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title(Span::styled(
                title,
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(para, area);
}

fn draw_git_panel(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.git_op_active || !app.git_output.is_empty() {
        // Show streaming output
        let available = area.height.saturating_sub(2) as usize; // borders
        let output = &app.git_output;
        let start = output.len().saturating_sub(available);
        for line_text in &output[start..] {
            let color = if line_text.starts_with("ERR:") { RED } else { TEXT };
            lines.push(Line::from(Span::styled(
                format!("  {}", line_text),
                Style::default().fg(color),
            )));
        }

        if !app.git_op_active {
            // Done — show full menu again below output
            let used = lines.len();
            if used + 2 <= available {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("  1", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
                    Span::styled("  Sync master   ", Style::default().fg(TEXT)),
                    Span::styled("2", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
                    Span::styled("  Rebase custom   ", Style::default().fg(TEXT)),
                    Span::styled("3", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
                    Span::styled("  Push custom   ", Style::default().fg(TEXT)),
                    Span::styled("Esc", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
                    Span::styled("  Close", Style::default().fg(TEXT)),
                ]));
            }
        }
    } else {
        // Idle — show menu
        let status_span = if let Some(ref gs) = app.git_status {
            let mut parts = vec![Span::styled(
                format!("  {}", gs.branch),
                Style::default().fg(OVERLAY0),
            )];
            if gs.ahead > 0 {
                parts.push(Span::styled(
                    format!(" | {} ahead of master", gs.ahead),
                    Style::default().fg(PEACH),
                ));
            }
            if gs.behind > 0 {
                parts.push(Span::styled(
                    format!(" | {} behind master", gs.behind),
                    Style::default().fg(YELLOW),
                ));
            }
            parts
        } else {
            vec![Span::styled(
                "  (git status unavailable)",
                Style::default().fg(OVERLAY0),
            )]
        };

        lines.push(Line::from(status_span));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  1", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
            Span::styled("  Sync master (fetch upstream)", Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  2", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
            Span::styled("  Rebase custom onto master", Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  3", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
            Span::styled("  Push custom (--force-with-lease)", Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Esc  Close",
            Style::default().fg(OVERLAY0),
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title(Span::styled(
                " Git Operations ",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(para, area);
}

fn draw_help_panel(f: &mut Frame, area: Rect) {
    let kb = |key: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:6}", key), Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
            Span::styled(desc, Style::default().fg(TEXT)),
        ])
    };
    let sep = || -> Line<'static> { Line::from("") };

    let lines = vec![
        kb("j/k", "Move up/down in list"),
        kb("/", "Search/filter packages"),
        kb("Tab", "Toggle dependency tree view"),
        kb("Enter", "Toggle detail panel"),
        sep(),
        kb("u", "Check upstream version (selected)"),
        kb("U", "Check upstream version (all)"),
        kb("t", "Bump template to upstream (selected)"),
        kb("T", "Bump template to upstream (all)"),
        kb("b", "Build selected package"),
        kb("B", "Build all buildable packages"),
        kb("c", "Clean old built packages (keep latest)"),
        kb("C", "Clean all built packages"),
        sep(),
        kb("g", "Git operations panel  (1: sync  2: rebase  3: push)"),
        kb("S", "Apply pending shlib updates"),
        kb("r", "Refresh package state"),
        kb("Esc", "Close panel / cancel build (press twice)"),
        kb("?", "Toggle this help panel"),
        kb("q", "Quit"),
    ];

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SURFACE0))
            .title(Span::styled(
                " Keybinds ",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(para, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let counts = app.status_counts();

    let mut spans = vec![
        Span::styled(" ", Style::default()),
    ];

    if counts.up_to_date > 0 {
        spans.push(Span::styled(format!("{} up to date", counts.up_to_date), Style::default().fg(GREEN)));
        spans.push(Span::styled(" │ ", Style::default().fg(OVERLAY0)));
    }
    if counts.upstream_ahead > 0 {
        spans.push(Span::styled(format!("{} upstream ahead", counts.upstream_ahead), Style::default().fg(PEACH)));
        spans.push(Span::styled(" │ ", Style::default().fg(OVERLAY0)));
    }
    if counts.build_outdated > 0 {
        spans.push(Span::styled(format!("{} build outdated", counts.build_outdated), Style::default().fg(YELLOW)));
        spans.push(Span::styled(" │ ", Style::default().fg(OVERLAY0)));
    }
    if counts.ready_to_install > 0 {
        spans.push(Span::styled(format!("{} ready to install", counts.ready_to_install), Style::default().fg(TEAL)));
        spans.push(Span::styled(" │ ", Style::default().fg(OVERLAY0)));
    }
    if counts.build_failed > 0 {
        spans.push(Span::styled(format!("{} failed", counts.build_failed), Style::default().fg(RED)));
        spans.push(Span::styled(" │ ", Style::default().fg(OVERLAY0)));
    }

    // Remove trailing separator
    if spans.len() > 1 {
        spans.pop();
    }

    // Add status message on the right
    if let Some(ref msg) = app.status_msg {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(msg.clone(), Style::default().fg(OVERLAY0)));
    }

    // Shlib update indicator
    if !app.shlib_updates.is_empty() {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            format!("{} shlib updates  S:apply", app.shlib_updates.len()),
            Style::default().fg(PEACH),
        ));
    }

    const KEYBINDS: &str = "j/k:nav  /:search  Tab:tree  u/U:check  t/T:bump  b/B:build  c/C:clean  g:git  ?:help  q:quit";

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(KEYBINDS.len() as u16)])
        .split(area);

    let left = Paragraph::new(Line::from(spans));
    f.render_widget(left, chunks[0]);

    let right = Paragraph::new(Span::styled(KEYBINDS, Style::default().fg(OVERLAY0)));
    f.render_widget(right, chunks[1]);
}
