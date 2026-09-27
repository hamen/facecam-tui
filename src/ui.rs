//! Drawing. Everything here reads the state; nothing changes it.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::{Resize, StatefulImage, protocol::StatefulProtocol};

use crate::{
    app::{App, BAR_MAX, FPS60_MAX, Focus, Preview},
    camera::Mode,
    capture,
};

/// Wide terminals put the preview on the left.
pub const WIDE: u16 = 120;
pub const PANEL_WIDTH: u16 = 52;
const PANEL_HEIGHT: u16 = 10;
/// Half-block previews are capped: past this they only cost time.
const HALFBLOCK_MAX: (u16, u16) = (96, 40);

/// `cell` is the terminal's cell size in pixels (width, height).
pub fn draw(
    frame: &mut Frame,
    app: &App,
    preview: Option<&mut StatefulProtocol>,
    halfblocks: bool,
    cell: (u16, u16),
) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let image = (capture::WIDTH, capture::HEIGHT);
    // The preview pane hugs the image; the panel sits right next to it (wide) or below it.
    let (preview_area, panel_area) = if main.width >= WIDE {
        let [p, _] =
            Layout::horizontal([Constraint::Min(10), Constraint::Length(PANEL_WIDTH)]).areas(main);
        let p = outer(preview_inner(inner(p), cell, image)).intersection(main);
        let c = Rect {
            x: p.right(),
            width: PANEL_WIDTH.min(main.right().saturating_sub(p.right())),
            ..main
        };
        (p, c)
    } else {
        let [p, _] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(PANEL_HEIGHT)]).areas(main);
        let p = outer(preview_inner(inner(p), cell, image)).intersection(main);
        let c = Rect {
            y: p.bottom(),
            height: PANEL_HEIGHT.min(main.bottom().saturating_sub(p.bottom())),
            ..main
        };
        (p, c)
    };

    draw_preview(frame, app, preview, halfblocks, preview_area);
    draw_panel(frame, app, panel_area);
    frame.render_widget(
        Paragraph::new(help(app)).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn draw_preview(
    frame: &mut Frame,
    app: &App,
    preview: Option<&mut StatefulProtocol>,
    halfblocks: bool,
    area: Rect,
) {
    let block = Block::default().borders(Borders::ALL).title(" Preview ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    match (preview, &app.preview) {
        (Some(protocol), Preview::Streaming) => {
            let area = if halfblocks {
                cap(inner, HALFBLOCK_MAX)
            } else {
                inner
            };
            // Scale, not the default Fit: Fit never enlarges, so a large pane showed a small
            // image. The half-block path keeps Fit behind its cap.
            let image = if halfblocks {
                StatefulImage::default()
            } else {
                StatefulImage::default().resize(Resize::Scale(None))
            };
            frame.render_stateful_widget(image, area, protocol);
        }
        (_, state) => {
            let text = match state {
                Preview::NoCamera => "no camera",
                Preview::Starting | Preview::Streaming => "starting preview…",
                Preview::Busy => "camera in use by another app — controls still work",
                Preview::Problem(e) => e.as_str(),
            };
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
        }
    }
}

/// The largest area inside `avail`, anchored top-left, whose size in pixels has the image's
/// aspect ratio, to the nearest whole cell.
pub fn preview_inner(avail: Rect, (cw, ch): (u16, u16), (iw, ih): (u32, u32)) -> Rect {
    if cw == 0 || ch == 0 || iw == 0 || ih == 0 {
        return avail;
    }
    let (cw, ch) = (u64::from(cw), u64::from(ch));
    let (iw, ih) = (u64::from(iw), u64::from(ih));
    let (w, h) = (u64::from(avail.width), u64::from(avail.height));
    // Full height: how many columns keep the aspect?
    let cols = (h * ch * iw + ih * cw / 2) / (ih * cw);
    let (cols, rows) = if cols <= w {
        (cols, h)
    } else {
        (w, ((w * cw * ih + iw * ch / 2) / (iw * ch)).min(h))
    };
    Rect {
        width: cols as u16,
        height: rows as u16,
        ..avail
    }
}

/// The area inside a one-cell border.
fn inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// The area with a one-cell border around `inner`.
fn outer(inner: Rect) -> Rect {
    Rect {
        x: inner.x.saturating_sub(1),
        y: inner.y.saturating_sub(1),
        width: inner.width + 2,
        height: inner.height + 2,
    }
}

fn cap(area: Rect, (w, h): (u16, u16)) -> Rect {
    Rect {
        width: area.width.min(w),
        height: area.height.min(h),
        ..area
    }
}

fn draw_panel(frame: &mut Frame, app: &App, area: Rect) {
    let title = match &app.device {
        Some(d) => format!(" Facecam {d} "),
        None => " Facecam ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(panel_lines(app, inner.width)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The panel's text. The control rows and notes fit `width`; the entry and message lines may be
/// longer (error text of any length) and wrap.
fn panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    // A row is the marker and label (13 columns), the bar, and the value text with its leading
    // space: 15 columns for the longest, `2500  250.0 ms`.
    let bar_width = width.saturating_sub(28).max(8) as usize;

    let mut lines = Vec::new();
    let exposure_text = match app.exposure {
        Some(v) => format!("{v:>4}  {:>5.1} ms", f64::from(v) / 10.0),
        None => "   –".to_string(),
    };
    let exposure_bar = app
        .exposure
        .map(|v| {
            bar(
                i64::from(v),
                1,
                i64::from(BAR_MAX),
                bar_width,
                &[100, 200, 300],
            )
        })
        .unwrap_or_else(|| " ".repeat(bar_width));
    lines.push(row(
        app,
        Focus::Exposure,
        "Exposure  ",
        &exposure_bar,
        &exposure_text,
    ));
    let note = app.exposure.and_then(exposure_note).unwrap_or("");
    lines.push(Line::from(Span::styled(
        format!("  {note}"),
        Style::default().fg(Color::Yellow),
    )));
    let flicker = app.exposure.and_then(flicker_note).unwrap_or("");
    lines.push(Line::from(Span::styled(
        format!("  {flicker}"),
        Style::default().fg(Color::Yellow),
    )));

    let (brightness_bar, brightness_text) = match (app.brightness, app.brightness_range) {
        (Some(v), Some((lo, hi))) => (bar(v, lo, hi, bar_width, &[]), format!("{v:>4}")),
        _ => (" ".repeat(bar_width), "   –".to_string()),
    };
    lines.push(row(
        app,
        Focus::Brightness,
        "Brightness",
        &brightness_bar,
        &brightness_text,
    ));

    let mode = match app.mode {
        Some(Mode::Auto) => "Auto",
        Some(Mode::Shutter) => "Shutter Priority",
        None => "–",
    };
    lines.push(row(app, Focus::Mode, "Mode      ", mode, ""));
    lines.push(Line::default());

    if let Some(entry) = &app.entry {
        lines.push(Line::from(Span::styled(
            format!("> {entry}_   Enter apply · Esc cancel"),
            Style::default().fg(Color::Cyan),
        )));
    } else if let Some(message) = &app.message {
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn row(app: &App, focus: Focus, label: &'static str, body: &str, value: &str) -> Line<'static> {
    let focused = app.focus == focus;
    let marker = if focused { "▶ " } else { "  " };
    let style = if focused {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(vec![
        Span::styled(format!("{marker}{label} "), style),
        Span::styled(
            body.to_string(),
            style.fg(if focused { Color::Cyan } else { Color::Blue }),
        ),
        Span::styled(format!(" {value}"), style),
    ])
}

fn help(app: &App) -> String {
    if app.entry.is_some() {
        return "digits · Backspace · Enter apply · Esc cancel · Ctrl+C quit".into();
    }
    "Tab focus · ←/→ ±1 · Shift ±10 · PgUp/PgDn ±100 · [ ] ±flicker-free (Exposure; below 100: up to 100) \
     · a auto · Enter type · r reload · q quit"
        .into()
}

/// A text slider. Values past `hi` draw full; `marks` show as ┃ on the empty part.
pub fn bar(value: i64, lo: i64, hi: i64, width: usize, marks: &[i64]) -> String {
    let span = (hi - lo).max(1);
    let pos = |v: i64| (((v.clamp(lo, hi) - lo) * width as i64 + span / 2) / span) as usize;
    let filled = pos(value);
    let mark_cells: Vec<usize> = marks
        .iter()
        .map(|m| pos(*m).min(width.saturating_sub(1)))
        .collect();
    (0..width)
        .map(|i| {
            if i < filled {
                '█'
            } else if mark_cells.contains(&i) {
                '┃'
            } else {
                '─'
            }
        })
        .collect()
}

/// The frame-rate warning for an exposure value (units of 100 µs).
/// Under 50 Hz light only multiples of 10 ms (100 units) stay flicker-free.
pub fn flicker_note(value: u32) -> Option<&'static str> {
    (!value.is_multiple_of(100)).then_some("flickers under 50 Hz light")
}

pub fn exposure_note(value: u32) -> Option<&'static str> {
    if value > BAR_MAX {
        Some("above 333: 30 fps drops too")
    } else if value > FPS60_MAX {
        Some("above 166: 60 fps apps (e.g. Meet) drop frames")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(bar(0, 0, 10, 10, &[]), "──────────");
        assert_eq!(bar(5, 0, 10, 10, &[]), "█████─────");
        assert_eq!(bar(10, 0, 10, 10, &[]), "██████████");
    }

    #[test]
    fn values_above_the_bar_draw_full() {
        assert_eq!(bar(2500, 1, 333, 10, &[]), "██████████");
    }

    #[test]
    fn marks_show_on_the_empty_part() {
        let b = bar(1, 1, 333, 30, &[100, 200, 300]);
        assert_eq!(b.chars().filter(|c| *c == '┃').count(), 3);
        let covered = bar(333, 1, 333, 30, &[100, 200, 300]);
        assert_eq!(covered.chars().filter(|c| *c == '┃').count(), 0);
    }

    const CELL: (u16, u16) = (12, 27);
    const IMAGE: (u32, u32) = (960, 540);

    #[test]
    fn the_preview_hugs_the_image() {
        let at = |w, h| Rect::new(3, 4, w, h);
        // Height-limited: 40 rows of 27 px hold 1080 px, so 1920 px = 160 columns.
        assert_eq!(preview_inner(at(200, 40), CELL, IMAGE), at(160, 40));
        // Width-limited: 80 columns of 12 px hold 960 px, so 540 px = 20 rows.
        assert_eq!(preview_inner(at(80, 40), CELL, IMAGE), at(80, 20));
        assert_eq!(preview_inner(at(0, 0), CELL, IMAGE), at(0, 0));
    }

    #[test]
    fn the_preview_keeps_the_aspect_within_a_cell() {
        let (cw, ch) = (u64::from(CELL.0), u64::from(CELL.1));
        let (iw, ih) = (u64::from(IMAGE.0), u64::from(IMAGE.1));
        for w in 1..=200 {
            for h in 1..=60 {
                let avail = Rect::new(0, 0, w, h);
                let r = preview_inner(avail, CELL, IMAGE);
                assert!(r.width <= w && r.height <= h, "{avail:?} -> {r:?}");
                assert!(r.width == w || r.height == h, "uses one full side: {r:?}");
                let (cols, rows) = (u64::from(r.width), u64::from(r.height));
                let skew = (cols * cw * ih).abs_diff(rows * ch * iw);
                assert!(skew <= (ih * cw).max(iw * ch), "{avail:?} -> {r:?}");
            }
        }
    }

    #[test]
    fn the_panel_sits_next_to_the_preview_at_any_size() {
        use ratatui::{Terminal, backend::TestBackend};
        let app = App::default();
        for w in (10..=220).step_by(7) {
            for h in (4..=70).step_by(3) {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| draw(f, &app, None, false, CELL)).unwrap();
                if w < WIDE || h < 12 {
                    continue;
                }
                let buffer = t.backend().buffer();
                let row: String = (0..w).map(|x| buffer[(x, 0)].symbol()).collect();
                let preview_end = row.find('┐').expect("preview corner");
                let rest = &row[preview_end + '┐'.len_utf8()..];
                assert!(
                    rest.starts_with('┌'),
                    "{w}x{h}: panel starts right after: {row}"
                );
            }
        }
    }

    #[test]
    fn flicker_notes() {
        assert_eq!(flicker_note(100), None);
        assert_eq!(flicker_note(200), None);
        assert_eq!(flicker_note(227), Some("flickers under 50 Hz light"));
        assert_eq!(flicker_note(1), Some("flickers under 50 Hz light"));
    }

    /// The control rows and notes (everything above the blank line) stay on one row each at the
    /// default panel width; only the entry and message lines may wrap.
    #[test]
    fn panel_rows_fit_the_default_panel() {
        let width = PANEL_WIDTH - 2;
        for exposure in [1, 200, 227, 2500] {
            for brightness in [0, 255] {
                for focus in [Focus::Exposure, Focus::Brightness, Focus::Mode] {
                    let mut app = App::default();
                    app.focus = focus;
                    app.exposure = Some(exposure);
                    app.exposure_range = Ok((1, 2500));
                    app.brightness = Some(brightness);
                    app.brightness_range = Some((0, 255));
                    app.mode = Some(Mode::Shutter);
                    let lines = panel_lines(&app, width);
                    let fixed: Vec<_> = lines.iter().take_while(|l| l.width() > 0).collect();
                    assert_eq!(fixed.len(), 5, "exposure, two notes, brightness, mode");
                    for line in fixed {
                        assert!(
                            line.width() <= usize::from(width),
                            "{exposure}/{brightness}: {} > {width}: {line}",
                            line.width()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn exposure_notes() {
        assert_eq!(exposure_note(166), None);
        assert!(exposure_note(167).unwrap().contains("60 fps"));
        assert!(exposure_note(333).unwrap().contains("60 fps"));
        assert!(exposure_note(334).unwrap().contains("30 fps"));
    }
}
