//! Egress tab (Horizon 3) — per-process egress profiles and policy drift.
//!
//! Shows what each process talks to (SNI / ASN / port) as learned by the
//! `EgressProfiler`, and — when an `egress-policy.toml` is loaded — whether
//! each destination is within the declared allowlist. `Shift+P` promotes the
//! current baseline into that policy. Read-only: the linter warns, it never
//! blocks.

use crate::app::App;
use crate::ui::widgets;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // table
            Constraint::Length(3), // footer
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_table(f, app, chunks[1]);
    render_footer(f, app, chunks[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let procs = app.egress_profiler.process_count();
    let dests = app.egress_profiler.dest_count();
    let (policy_text, policy_style) = if app.egress_profiler.has_policy() {
        (
            "policy: loaded — warn on drift",
            Style::default().fg(t.status_good),
        )
    } else {
        (
            "policy: none (observe only)",
            Style::default().fg(t.text_muted),
        )
    };
    let extra = vec![
        Span::raw("  "),
        Span::styled("EGRESS", Style::default().fg(t.brand).bold()),
        Span::raw(format!("  {procs} processes · {dests} destinations  ")),
        Span::styled(policy_text, policy_style),
    ];
    widgets::render_header_with_extra(f, app, area, extra);
}

fn render_table(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let profiles = app.egress_profiler.snapshot();

    if profiles.is_empty() {
        let empty = Paragraph::new(
            "No egress observed yet.\n\nTraffic to external hosts will appear here, grouped by \
             process. SNI is read from the cleartext TLS/QUIC ClientHello — no decryption needed.",
        )
        .style(Style::default().fg(t.text_muted))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.border)),
        );
        f.render_widget(empty, area);
        return;
    }

    // Flatten profiles → rows, processes alphabetical, destinations by hit
    // count (descending). `egress_scroll` is the *selected* flat index
    // (wheel / arrows via `scroll_tab`, clamped against `dest_count`); the
    // visible window follows the selection, processes-tab style.
    let body_rows = (area.height.saturating_sub(3)) as usize; // borders + header row
    let grand_total = app.egress_profiler.dest_count();
    let selected = app
        .ui
        .scroll
        .egress_scroll
        .min(grand_total.saturating_sub(1));
    let start = (selected + 1).saturating_sub(body_rows);
    let now = std::time::SystemTime::now();
    let mut rows: Vec<Row> = Vec::new();
    let mut shown = 0usize;
    let mut total = 0usize;

    for profile in &profiles {
        let mut dests: Vec<_> = profile.dests.values().collect();
        dests.sort_by_key(|d| std::cmp::Reverse(d.count));
        for dest in dests {
            total += 1;
            if total <= start || shown >= body_rows {
                continue;
            }
            let is_selected = total - 1 == selected;
            let name = dest
                .sni
                .clone()
                .or_else(|| dest.asn_org.clone())
                .unwrap_or_else(|| "(ip)".to_string());
            let (verdict, vstyle) = match app.egress_profiler.dest_allowed(&profile.process, dest) {
                Some(true) => ("✓ ok", Style::default().fg(t.status_good)),
                Some(false) => ("✗ drift", Style::default().fg(t.status_error).bold()),
                None => ("—", Style::default().fg(t.text_muted)),
            };
            let asn = dest.asn_org.clone().unwrap_or_default();
            let row = Row::new(vec![
                Cell::from(profile.process.clone()).style(Style::default().fg(t.text_primary)),
                Cell::from(name).style(Style::default().fg(t.text_primary)),
                Cell::from(dest.port.to_string()).style(Style::default().fg(t.text_secondary)),
                Cell::from(asn).style(Style::default().fg(t.text_muted)),
                Cell::from(dest.count.to_string()).style(Style::default().fg(t.text_secondary)),
                Cell::from(fmt_age(dest.first_seen, now)).style(Style::default().fg(t.text_muted)),
                Cell::from(fmt_age(dest.last_seen, now))
                    .style(Style::default().fg(t.text_secondary)),
                Cell::from(verdict).style(vstyle),
            ]);
            rows.push(if is_selected {
                row.style(Style::default().bg(t.selection_bg))
            } else {
                row
            });
            shown += 1;
        }
    }

    let header = Row::new(vec![
        "Process",
        "Destination",
        "Port",
        "ASN org",
        "Seen",
        "First",
        "Last",
        "Policy",
    ])
    .style(Style::default().fg(t.key_hint).bold());

    let title = if total > shown || start > 0 {
        let first = (start + 1).min(total);
        format!(" Egress profiles  ({first}–{} of {total}) ", start + shown)
    } else {
        format!(" Egress profiles  ({total}) ")
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(16),
            Constraint::Percentage(30),
            Constraint::Length(6),
            Constraint::Percentage(20),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.border))
            .title(title),
    );
    f.render_widget(table, area);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let line = Line::from(vec![
        Span::styled("Enter", Style::default().fg(t.key_hint).bold()),
        Span::raw(" promote selected process   "),
        Span::styled("Shift+P", Style::default().fg(t.key_hint).bold()),
        Span::raw(" promote all → egress-policy.toml   "),
        Span::styled("✗ drift", Style::default().fg(t.status_error)),
        Span::raw(" = outside allowlist (warns, never blocks)"),
    ]);
    let footer = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(t.border)),
    );
    f.render_widget(footer, area);
}

/// The process owning the selected row — flat index over the same ordering
/// the table renders (processes alphabetical; any dest row selects its
/// process, so per-dest ordering within a profile doesn't matter here).
pub fn selected_process(app: &App) -> Option<String> {
    let profiles = app.egress_profiler.snapshot();
    let selected = app.ui.scroll.egress_scroll;
    let mut idx = 0usize;
    for profile in &profiles {
        let n = profile.dests.len();
        if selected < idx + n {
            return Some(profile.process.clone());
        }
        idx += n;
    }
    // Selection clamped past the end (list shrank) → last process.
    profiles.last().map(|p| p.process.clone())
}

/// Compact age like `34s`, `5m`, `3h`, `2d`.
fn fmt_age(t: std::time::SystemTime, now: std::time::SystemTime) -> String {
    let secs = now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}
