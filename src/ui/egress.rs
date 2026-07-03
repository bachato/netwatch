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
    // count (descending). Scroll offset indexes into the flattened list
    // (wheel / arrows via `scroll_tab`, clamped against `dest_count`).
    let body_rows = (area.height.saturating_sub(3)) as usize; // borders + header row
    let offset = app.ui.scroll.egress_scroll;
    let mut rows: Vec<Row> = Vec::new();
    let mut shown = 0usize;
    let mut total = 0usize;

    for profile in &profiles {
        let mut dests: Vec<_> = profile.dests.values().collect();
        dests.sort_by(|a, b| b.count.cmp(&a.count));
        for dest in dests {
            total += 1;
            if total <= offset || shown >= body_rows {
                continue;
            }
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
            rows.push(Row::new(vec![
                Cell::from(profile.process.clone()).style(Style::default().fg(t.text_primary)),
                Cell::from(name).style(Style::default().fg(t.text_primary)),
                Cell::from(dest.port.to_string()).style(Style::default().fg(t.text_secondary)),
                Cell::from(asn).style(Style::default().fg(t.text_muted)),
                Cell::from(dest.count.to_string()).style(Style::default().fg(t.text_secondary)),
                Cell::from(verdict).style(vstyle),
            ]));
            shown += 1;
        }
    }

    let header = Row::new(vec![
        "Process",
        "Destination",
        "Port",
        "ASN org",
        "Seen",
        "Policy",
    ])
    .style(Style::default().fg(t.key_hint).bold());

    let title = if total > shown || offset > 0 {
        let first = (offset + 1).min(total);
        format!(" Egress profiles  ({first}–{} of {total}) ", offset + shown)
    } else {
        format!(" Egress profiles  ({total}) ")
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(18),
            Constraint::Percentage(34),
            Constraint::Length(6),
            Constraint::Percentage(24),
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
        Span::styled("Shift+P", Style::default().fg(t.key_hint).bold()),
        Span::raw(" promote baseline → egress-policy.toml   "),
        Span::styled("✗ drift", Style::default().fg(t.status_error)),
        Span::raw(" = flow outside the declared allowlist (warns, never blocks)"),
    ]);
    let footer = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(t.border)),
    );
    f.render_widget(footer, area);
}
