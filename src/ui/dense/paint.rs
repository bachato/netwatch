//! Drawing primitives for the Dense view.
//!
//! Three things the rest of the UI has no use for, and which the Dense layout
//! is built out of:
//!
//! 1. [`area_graph`] — a braille area plot that addresses the **two
//!    sub-columns of a cell independently**, so a 120-column graph carries 240
//!    samples. [`crate::graph::render_dots`] deliberately fills both
//!    sub-columns to get a solid area at 4× vertical resolution; this one
//!    trades that vertical resolution for horizontal samples, and gets the
//!    magnitude back as colour. It also draws **mirrored** (`flip`), which is
//!    what puts upload below the shared time axis.
//! 2. [`meter`] — a bounded horizontal bar whose ramp is sampled by *position*,
//!    so the far end is the alarm colour before the value ever reaches it.
//! 3. [`panel`] — a rounded box that carries its own metadata in the border:
//!    hotkey, title, sub-label and right-hand info on the top edge, keybinds
//!    and paging on the bottom. This is what buys the layout its "no chrome
//!    rows" property — a heading that costs no row.
//!
//! Colours arrive as a [`Ramps`] built from the active [`Theme`]. Nothing here
//! hardcodes a hex value: the design handoff specifies literal ramps, but
//! honouring the user's theme outranks matching the reference pixel-for-pixel,
//! and the same rule already governs Lite. On 16-colour themes
//! ([`Theme::defers_to_terminal`]) every ramp collapses to a flat token —
//! synthesising 24-bit gradients there would ignore the user's palette, which
//! is the same call `graph::fade_color` already makes.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::graph::{fade_color, BRAILLE_BASE, BRAILLE_BIT};
use crate::theme::Theme;

// ── ramps ───────────────────────────────────────────────────────────────────

/// A five-stop colour ramp, sampled 0.0..=1.0.
///
/// Flat ramps (one distinct stop) are the 16-colour degrade path and are not a
/// special case anywhere else — [`Ramp::at`] just always returns the token.
#[derive(Debug, Clone)]
pub struct Ramp {
    stops: Vec<Color>,
}

impl Ramp {
    pub fn flat(c: Color) -> Self {
        Self { stops: vec![c] }
    }

    /// Sample at `f`, clamped to 0.0..=1.0.
    pub fn at(&self, f: f32) -> Color {
        match self.stops.len() {
            0 => Color::Reset,
            1 => self.stops[0],
            n => {
                let t = f.clamp(0.0, 1.0) * (n - 1) as f32;
                let i = t.floor() as usize;
                if i >= n - 1 {
                    return self.stops[n - 1];
                }
                lerp(self.stops[i], self.stops[i + 1], t - i as f32)
            }
        }
    }

    /// The ramp's midpoint — used where a single representative colour is
    /// wanted (a legend, a label above the graph it describes).
    pub fn mid(&self) -> Color {
        self.at(0.55)
    }
}

/// The four ramps the Dense view draws with.
///
/// The split between magnitude and severity is the one deliberate departure
/// from btop, and it matters: btop's graphs run green→amber→red because CPU
/// load genuinely gets worse as it climbs. **Bandwidth doesn't.** A saturated
/// link during a backup is working, not failing. So throughput graphs ramp
/// cool→bright (busy), and only bounded values — link saturation, latency
/// budget — get the green→amber→red vocabulary.
#[derive(Debug, Clone)]
pub struct Ramps {
    /// Download magnitude. High = busy.
    pub down: Ramp,
    /// Upload magnitude. High = busy.
    pub up: Ramp,
    /// Bounded values where high genuinely IS bad. Meters only.
    pub load: Ramp,
    /// Present but not participating (down/idle interfaces, dead rows).
    pub dim: Ramp,
}

impl Ramps {
    pub fn from_theme(t: &Theme) -> Self {
        if t.defers_to_terminal() {
            return Self {
                down: Ramp::flat(t.rx_rate),
                up: Ramp::flat(t.tx_rate),
                load: Ramp::flat(t.status_warn),
                dim: Ramp::flat(t.text_muted),
            };
        }
        Self {
            down: magnitude_ramp(t.rx_rate, t.bg),
            up: magnitude_ramp(t.tx_rate, t.bg),
            load: Ramp {
                stops: vec![
                    t.status_good,
                    lerp(t.status_good, t.status_warn, 0.5),
                    t.status_warn,
                    lerp(t.status_warn, t.status_error, 0.5),
                    t.status_error,
                ],
            },
            dim: Ramp {
                stops: vec![
                    fade_color(t.text_muted, t.bg, 0.45, false),
                    t.text_muted,
                    fade_color(t.text_secondary, t.bg, 0.85, false),
                ],
            },
        }
    }
}

/// Deep-and-cool at the baseline through bright at the peak, anchored on a
/// theme token. Five stops so the eye can rank a spike without an axis.
fn magnitude_ramp(base: Color, bg: Color) -> Ramp {
    Ramp {
        stops: vec![
            fade_color(base, bg, 0.30, false),
            fade_color(base, bg, 0.62, false),
            base,
            lighten(base, 0.42),
            lighten(base, 0.76),
        ],
    }
}

fn rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Linear blend, RGB only. Non-RGB inputs return `a` unchanged rather than
/// guessing at a palette entry's actual value — the same discipline as
/// [`crate::graph::fade_color`].
fn lerp(a: Color, b: Color, f: f32) -> Color {
    match (rgb(a), rgb(b)) {
        (Some((ar, ag, ab)), Some((br, bg_, bb))) => {
            let f = f.clamp(0.0, 1.0);
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg_), mix(ab, bb))
        }
        _ => a,
    }
}

fn lighten(c: Color, f: f32) -> Color {
    lerp(c, Color::Rgb(255, 255, 255), f)
}

// ── braille area graph ──────────────────────────────────────────────────────

/// Sub-cell height of `v` against `max`, rounded.
fn sub_height(v: u64, max: u64, sub_h: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let v = v.min(max) as u128;
    // Round half up in integer maths: (v * sub_h + max/2) / max.
    ((v * sub_h as u128 * 2 + max as u128) / (max as u128 * 2)) as usize
}

/// Braille area plot at two samples per cell column.
///
/// `samples` is oldest-first and must already be exactly `2 × area.width`
/// long — see [`super::resample`], which is where the bucketing lives so that
/// a spike between samples survives instead of being dropped by decimation.
///
/// `flip` grows the plot **downward from the top edge** instead of upward from
/// the bottom. That is the whole mirrored-graph trick: the upload half is the
/// same function with `flip = true`, sharing the axis row above it.
///
/// Every cell is coloured by its own height in the plot rather than by which
/// series it belongs to, so severity is pre-attentive — you see the spike
/// before you read the axis.
pub fn area_graph(
    buf: &mut Buffer,
    area: Rect,
    samples: &[u64],
    max: u64,
    ramp: &Ramp,
    flip: bool,
) {
    if area.width == 0 || area.height == 0 || max == 0 {
        return;
    }
    let w = area.width as usize;
    let h = area.height as usize;
    let sub_h = h * 4;

    for cx in 0..w {
        let lh = sub_height(samples.get(cx * 2).copied().unwrap_or(0), max, sub_h);
        let rh = sub_height(samples.get(cx * 2 + 1).copied().unwrap_or(0), max, sub_h);
        // A zero sample still draws its baseline dot. Skipping it leaves holes
        // in the area wherever traffic went quiet, which reads as "no data"
        // rather than "no traffic" — and it is what stops this looking like
        // btop, whose plot is continuous across the whole window.
        let lh = lh.max(1);
        let rh = rh.max(1);
        for cy in 0..h {
            let mut bits: u8 = 0;
            for (s, (l_dot, r_dot)) in BRAILLE_BIT[0].iter().zip(BRAILLE_BIT[1]).enumerate() {
                let from_top = cy * 4 + s;
                // Depth of this sub-row measured from the growing edge.
                let depth = if flip { from_top + 1 } else { sub_h - from_top };
                if lh >= depth {
                    bits |= 1 << l_dot;
                }
                if rh >= depth {
                    bits |= 1 << r_dot;
                }
            }
            if bits == 0 {
                continue;
            }
            // Sample the ramp at the cell's vertical midpoint, measured along
            // the direction of growth, so both halves of a mirrored pair
            // brighten as traffic climbs.
            let mid = (cy * 4 + 2) as f32;
            let f = if flip {
                mid / sub_h as f32
            } else {
                (sub_h as f32 - mid) / sub_h as f32
            };
            let Some(ch) = char::from_u32(BRAILLE_BASE | bits as u32) else {
                continue;
            };
            let cell = buf.get_mut(area.x + cx as u16, area.y + cy as u16);
            cell.set_char(ch);
            cell.set_style(Style::default().fg(ramp.at(f)));
        }
    }
}

/// Single-row sparkline: [`area_graph`] at height 1.
pub fn spark(buf: &mut Buffer, x: u16, y: u16, w: u16, samples: &[u64], max: u64, ramp: &Ramp) {
    area_graph(buf, Rect::new(x, y, w, 1), samples, max, ramp, false);
}

/// A flat baseline for a series that is present but not moving — an idle or
/// down interface, a `TIME_WAIT` row. Drawing nothing would read as "no such
/// interface"; a flat line reads as "nothing happening", which is the truth.
pub fn baseline(buf: &mut Buffer, x: u16, y: u16, w: u16, color: Color) {
    for i in 0..w {
        let cell = buf.get_mut(x + i, y);
        cell.set_char('⣀');
        cell.set_style(Style::default().fg(color));
    }
}

// ── meter ───────────────────────────────────────────────────────────────────

/// Bounded-value meter, btop's mem-bar idiom.
///
/// The ramp is sampled by **position along the bar**, not by value, so the far
/// end is always the alarm colour even when the value never reaches it — you
/// learn where the danger zone is before you're in it.
///
/// Callers must leave room for whatever label follows: a meter that runs under
/// its own readout appears to *shrink* as it fills, which is a lie told at
/// precisely the moment the number matters.
pub fn meter(buf: &mut Buffer, x: u16, y: u16, w: u16, frac: f32, ramp: &Ramp, empty: Color) {
    if w == 0 {
        return;
    }
    let filled = (frac.clamp(0.0, 1.0) * w as f32).round() as u16;
    // A 1-cell meter has no span to interpolate across; sample the ramp's
    // origin rather than dividing by zero.
    let span = (w.saturating_sub(1)).max(1) as f32;
    for i in 0..w {
        let on = i < filled;
        let cell = buf.get_mut(x + i, y);
        cell.set_char(if on { '■' } else { '·' });
        cell.set_style(Style::default().fg(if on { ramp.at(i as f32 / span) } else { empty }));
    }
}

// ── panel ───────────────────────────────────────────────────────────────────

const TL: char = '╭';
const TR: char = '╮';
const BL: char = '╰';
const BR: char = '╯';
const H: char = '─';
const V: char = '│';

/// A keybind segment for the bottom border: `(key, rest)`, drawn as an accented
/// key followed by dim text — `q` + `uit`, `↑↓` + ` select`.
pub type Bind<'a> = (&'a str, &'a str);

/// Everything a [`panel`] carries in its own border.
#[derive(Default)]
pub struct PanelOpts<'a> {
    /// Bracketed hotkey at the top-left: the `1` in `╭─┤1├─┤ net ├`.
    pub key: Option<&'a str>,
    pub title: Option<&'a str>,
    /// Dim qualifier after the title — the interface, the row count.
    pub sub: Option<&'a str>,
    /// Right-hand info on the top border.
    pub right: Option<&'a str>,
    pub right_style: Option<Style>,
    /// Keybind strip on the bottom border.
    pub foot_left: &'a [Bind<'a>],
    /// Paging / range on the bottom border.
    pub foot_right: Option<&'a str>,
}

/// Draw a rounded panel and return its **interior** rect.
///
/// Rounded hairline corners read as soft furniture rather than a grid of cages
/// — the reason btop looks modern and `dialog` looks like 1994.
pub fn panel(buf: &mut Buffer, area: Rect, t: &Theme, o: &PanelOpts) -> Rect {
    if area.width < 2 || area.height < 2 {
        return area;
    }
    let border = Style::default().fg(t.border);
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;

    for x in (x0 + 1)..x1 {
        set(buf, x, y0, H, border);
        set(buf, x, y1, H, border);
    }
    for y in (y0 + 1)..y1 {
        set(buf, x0, y, V, border);
        set(buf, x1, y, V, border);
    }
    set(buf, x0, y0, TL, border);
    set(buf, x1, y0, TR, border);
    set(buf, x0, y1, BL, border);
    set(buf, x1, y1, BR, border);

    // ── top border inserts ──
    let mut cx = x0 + 1;
    if o.key.is_some() || o.title.is_some() {
        // One rule cell before the first bracket, so a corner never butts an
        // insert.
        cx = put(buf, cx, y0, "─", border);
    }
    if let Some(k) = o.key {
        cx = put(buf, cx, y0, "┤", border);
        cx = put(
            buf,
            cx,
            y0,
            k,
            Style::default().fg(t.key_hint).add_modifier(Modifier::BOLD),
        );
        cx = put(buf, cx, y0, "├─", border);
    }
    if let Some(title) = o.title {
        cx = put(buf, cx, y0, "┤ ", border);
        cx = put(
            buf,
            cx,
            y0,
            title,
            Style::default().fg(t.brand).add_modifier(Modifier::BOLD),
        );
        cx = put(buf, cx, y0, " ├", border);
    }
    if let Some(sub) = o.sub {
        cx = put(buf, cx, y0, "─┤ ", border);
        cx = put(buf, cx, y0, sub, Style::default().fg(t.text_muted));
        let _ = put(buf, cx, y0, " ├", border);
    }
    if let Some(right) = o.right {
        let style = o
            .right_style
            .unwrap_or_else(|| Style::default().fg(t.text_muted));
        insert_right(buf, x1, y0, right, style, border);
    }

    // ── bottom border inserts ──
    if !o.foot_left.is_empty() {
        let mut fx = x0 + 2;
        fx = put(buf, fx, y1, "┤ ", border);
        for (i, (key, rest)) in o.foot_left.iter().enumerate() {
            if i > 0 {
                fx = put(buf, fx, y1, "  ", border);
            }
            fx = put(
                buf,
                fx,
                y1,
                key,
                Style::default().fg(t.key_hint).add_modifier(Modifier::BOLD),
            );
            fx = put(buf, fx, y1, rest, Style::default().fg(t.text_muted));
        }
        let _ = put(buf, fx, y1, " ├", border);
    }
    if let Some(fr) = o.foot_right {
        insert_right(buf, x1, y1, fr, Style::default().fg(t.text_muted), border);
    }

    Rect::new(x0 + 1, y0 + 1, area.width - 2, area.height - 2)
}

/// `┤ text ├` ending one rule cell short of the corner at `x1`.
fn insert_right(buf: &mut Buffer, x1: u16, y: u16, text: &str, style: Style, border: Style) {
    let w = text.width() as u16 + 4; // ┤ + space + text + space + ├
    if w + 2 > x1 {
        return;
    }
    let x = x1 - 1 - w;
    let mut cx = put(buf, x, y, "┤ ", border);
    cx = put(buf, cx, y, text, style);
    let _ = put(buf, cx, y, " ├", border);
}

fn set(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if x >= buf.area.right() || y >= buf.area.bottom() {
        return;
    }
    let cell = buf.get_mut(x, y);
    cell.set_char(ch);
    cell.set_style(style);
}

/// Write `s` at `(x, y)` and return the column after it.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) -> u16 {
    if x >= buf.area.right() || y >= buf.area.bottom() {
        return x;
    }
    let max = (buf.area.right() - x) as usize;
    buf.set_stringn(x, y, s, max, style);
    x + s.width() as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn ch(b: &Buffer, x: u16, y: u16) -> char {
        b.get(x, y).symbol().chars().next().unwrap_or(' ')
    }

    fn row(b: &Buffer, y: u16) -> String {
        (0..b.area.width).map(|x| ch(b, x, y)).collect()
    }

    /// The two sub-columns must be independently addressable — that is the
    /// entire point of this graph over `graph::render_dots`. Two different
    /// samples in one cell must produce an asymmetric glyph. (A zero sample
    /// still contributes its baseline dot, so "empty" is one dot, not none.)
    #[test]
    fn two_samples_share_one_cell() {
        let mut b = buf(1, 1);
        // left full (4 sub-rows), right at baseline.
        area_graph(
            &mut b,
            Rect::new(0, 0, 1, 1),
            &[4, 0],
            4,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(ch(&b, 0, 0), '⣇', "left column full, right on the baseline");

        let mut b = buf(1, 1);
        area_graph(
            &mut b,
            Rect::new(0, 0, 1, 1),
            &[0, 4],
            4,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(ch(&b, 0, 0), '⣸', "right column full, left on the baseline");

        let mut b = buf(1, 1);
        area_graph(
            &mut b,
            Rect::new(0, 0, 1, 1),
            &[4, 4],
            4,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(ch(&b, 0, 0), '⣿', "both full");
    }

    /// A single sub-unit must light the row nearest the growing edge, and the
    /// flipped graph must light the opposite one. If these two ever agree, the
    /// mirrored graph has stopped mirroring.
    #[test]
    fn flip_grows_from_the_other_edge() {
        let mut up = buf(1, 1);
        area_graph(
            &mut up,
            Rect::new(0, 0, 1, 1),
            &[1, 1],
            4,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(ch(&up, 0, 0), '⣀', "unflipped: lowest dots");

        let mut down = buf(1, 1);
        area_graph(
            &mut down,
            Rect::new(0, 0, 1, 1),
            &[1, 1],
            4,
            &Ramp::flat(Color::Red),
            true,
        );
        assert_eq!(ch(&down, 0, 0), '⠉', "flipped: highest dots");
    }

    /// An idle window must still draw a continuous line at the axis. Gaps in
    /// the plot are the difference between "quiet" and "broken".
    #[test]
    fn quiet_traffic_draws_a_continuous_baseline() {
        let mut b = buf(6, 2);
        area_graph(
            &mut b,
            Rect::new(0, 0, 6, 2),
            &[0; 12],
            100,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(row(&b, 1), "⣀⣀⣀⣀⣀⣀", "every column carries a baseline");
        assert_eq!(row(&b, 0), "      ", "and nothing above it");

        let mut b = buf(6, 2);
        area_graph(
            &mut b,
            Rect::new(0, 0, 6, 2),
            &[0; 12],
            100,
            &Ramp::flat(Color::Red),
            true,
        );
        assert_eq!(row(&b, 0), "⠉⠉⠉⠉⠉⠉", "mirrored: baseline sits at the top");
    }

    #[test]
    fn full_scale_fills_every_row_of_a_tall_graph() {
        let mut b = buf(1, 3);
        area_graph(
            &mut b,
            Rect::new(0, 0, 1, 3),
            &[10, 10],
            10,
            &Ramp::flat(Color::Red),
            false,
        );
        for y in 0..3 {
            assert_eq!(ch(&b, 0, y), '⣿', "row {y} of a full-scale column");
        }
    }

    /// Colour must track height, not series — the claim the whole design rests
    /// on. Sampled at the cell midpoint, so a tall column's top cell is
    /// brighter than its bottom cell.
    #[test]
    fn gradient_tracks_height_in_both_directions() {
        let ramp = Ramp {
            stops: vec![Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)],
        };
        let mut b = buf(1, 4);
        area_graph(&mut b, Rect::new(0, 0, 1, 4), &[16, 16], 16, &ramp, false);
        let top = b.get(0, 0).fg;
        let bottom = b.get(0, 3).fg;
        assert!(
            matches!((top, bottom), (Color::Rgb(a, _, _), Color::Rgb(c, _, _)) if a > c),
            "unflipped: top cell must be the bright end, got {top:?} over {bottom:?}"
        );

        let mut b = buf(1, 4);
        area_graph(&mut b, Rect::new(0, 0, 1, 4), &[16, 16], 16, &ramp, true);
        let top = b.get(0, 0).fg;
        let bottom = b.get(0, 3).fg;
        assert!(
            matches!((top, bottom), (Color::Rgb(a, _, _), Color::Rgb(c, _, _)) if c > a),
            "flipped: bottom cell must be the bright end, got {top:?} under {bottom:?}"
        );
    }

    #[test]
    fn zero_max_and_zero_width_draw_nothing() {
        let mut b = buf(4, 1);
        area_graph(
            &mut b,
            Rect::new(0, 0, 4, 1),
            &[1, 2, 3, 4],
            0,
            &Ramp::flat(Color::Red),
            false,
        );
        assert_eq!(row(&b, 0), "    ", "max=0 must not divide by zero or paint");
    }

    /// The degenerate width that threw in the reference implementation.
    #[test]
    fn meter_handles_one_cell() {
        let mut b = buf(1, 1);
        meter(&mut b, 0, 0, 1, 0.5, &Ramp::flat(Color::Red), Color::Gray);
        assert_eq!(ch(&b, 0, 0), '■');
    }

    #[test]
    fn meter_fills_proportionally_and_ramps_by_position() {
        let ramp = Ramp {
            stops: vec![Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)],
        };
        let mut b = buf(10, 1);
        meter(&mut b, 0, 0, 10, 0.5, &ramp, Color::Gray);
        assert_eq!(row(&b, 0), "■■■■■·····");
        // Position-sampled: cell 4 is mid-ramp even though the value stops there.
        assert_ne!(b.get(0, 0).fg, b.get(4, 0).fg);
    }

    #[test]
    fn panel_draws_rounded_corners_and_returns_its_interior() {
        let t = crate::theme::by_name("dark");
        let mut b = buf(20, 4);
        let inner = panel(&mut b, Rect::new(0, 0, 20, 4), &t, &PanelOpts::default());
        assert_eq!(ch(&b, 0, 0), '╭');
        assert_eq!(ch(&b, 19, 0), '╮');
        assert_eq!(ch(&b, 0, 3), '╰');
        assert_eq!(ch(&b, 19, 3), '╯');
        assert_eq!(inner, Rect::new(1, 1, 18, 2));
    }

    /// The border-embedded title is what buys the layout its zero chrome rows,
    /// so its exact shape is part of the design, not decoration.
    #[test]
    fn panel_title_lives_in_the_border() {
        let t = crate::theme::by_name("dark");
        let mut b = buf(40, 3);
        panel(
            &mut b,
            Rect::new(0, 0, 40, 3),
            &t,
            &PanelOpts {
                key: Some("1"),
                title: Some("net"),
                sub: Some("eth0"),
                right: Some("up 4d"),
                ..Default::default()
            },
        );
        let top = row(&b, 0);
        assert!(
            top.starts_with("╭─┤1├─┤ net ├─┤ eth0 ├"),
            "top border was {top:?}"
        );
        assert!(top.ends_with("┤ up 4d ├─╮"), "top border was {top:?}");
    }

    #[test]
    fn panel_keybinds_live_in_the_bottom_border() {
        let t = crate::theme::by_name("dark");
        let mut b = buf(40, 3);
        panel(
            &mut b,
            Rect::new(0, 0, 40, 3),
            &t,
            &PanelOpts {
                foot_left: &[("q", "uit"), ("↵", " detail")],
                foot_right: Some("1-9 of 24"),
                ..Default::default()
            },
        );
        let bottom = row(&b, 2);
        assert!(
            bottom.starts_with("╰─┤ quit  ↵ detail ├"),
            "bottom border was {bottom:?}"
        );
        assert!(
            bottom.ends_with("┤ 1-9 of 24 ├─╯"),
            "bottom border was {bottom:?}"
        );
    }

    /// A panel too narrow for its own right-hand insert must drop the insert,
    /// not wrap it onto the border or panic on the subtraction.
    #[test]
    fn panel_drops_inserts_that_do_not_fit() {
        let t = crate::theme::by_name("dark");
        let mut b = buf(8, 3);
        panel(
            &mut b,
            Rect::new(0, 0, 8, 3),
            &t,
            &PanelOpts {
                right: Some("a very long string indeed"),
                ..Default::default()
            },
        );
        assert_eq!(ch(&b, 7, 0), '╮', "corner survives");
    }

    #[test]
    fn ramps_collapse_to_flat_on_terminal_palette_themes() {
        // `terminal` defers to the user's 16 colours; synthesising a gradient
        // there would ignore the palette it exists to honour.
        let t = crate::theme::by_name("terminal");
        assert!(t.defers_to_terminal());
        let r = Ramps::from_theme(&t);
        assert_eq!(r.down.at(0.0), r.down.at(1.0));
    }

    #[test]
    fn rgb_ramps_actually_ramp() {
        let t = crate::theme::by_name("dark");
        let r = Ramps::from_theme(&t);
        assert_ne!(r.down.at(0.0), r.down.at(1.0));
        assert_ne!(r.load.at(0.0), r.load.at(1.0));
    }
}
