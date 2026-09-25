//! Drawing. Everything here reads the state; nothing changes it.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};

use crate::{
    app::{App, BAR_MAX, FPS60_MAX, Focus, Preview},
    camera::Mode,
};

/// Wide terminals put the preview on the left.
const WIDE: u16 = 120;
const PANEL_WIDTH: u16 = 52;
const PANEL_HEIGHT: u16 = 10;
/// Half-block previews are capped: past this they only cost time.
const HALFBLOCK_MAX: (u16, u16) = (96, 40);

pub fn draw(
    frame: &mut Frame,
    app: &App,
    preview: Option<&mut StatefulProtocol>,
    halfblocks: bool,
) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let (preview_area, panel_area) = if main.width >= WIDE {
        let [p, c] =
            Layout::horizontal([Constraint::Min(10), Constraint::Length(PANEL_WIDTH)]).areas(main);
        (p, c)
    } else {
        let [p, c] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(PANEL_HEIGHT)]).areas(main);
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
            frame.render_stateful_widget(StatefulImage::default(), area, protocol);
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
    let bar_width = inner.width.saturating_sub(24).max(8) as usize;

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
        format!("          {note}"),
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
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn row<'a>(app: &App, focus: Focus, label: &'a str, body: &str, value: &str) -> Line<'a> {
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

    #[test]
    fn exposure_notes() {
        assert_eq!(exposure_note(166), None);
        assert!(exposure_note(167).unwrap().contains("60 fps"));
        assert!(exposure_note(333).unwrap().contains("60 fps"));
        assert!(exposure_note(334).unwrap().contains("30 fps"));
    }
}
