//! Fits the kitty window to the preview and the panel at startup, and gives the window its size
//! back on exit. X11 only: kitty refuses the xterm resize sequence (`CSI 8 ; rows ; cols t`), so
//! this uses `xdotool` on the window id kitty exports as `$WINDOWID`, and `xprop` to leave a
//! maximized or fullscreen window alone.

use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

use crate::{
    capture,
    ui::{PANEL_WIDTH, WIDE},
};

/// How long to wait for the window manager to apply the resize before reading the size back.
const SETTLE: Duration = Duration::from_millis(1000);

/// A window this process resized.
#[derive(Debug)]
pub struct Fitted {
    window_id: String,
    original: (u32, u32),
    /// The size read back after the resize, not the size asked for: a window manager may clamp it.
    fitted: (u32, u32),
}

/// Fits the window when this is kitty itself on X11, outside tmux, and the window is neither
/// maximized nor fullscreen. `Ok(None)`: nothing to do. `Err`: a message for the panel.
pub fn fit(protocol_kitty: bool, tmux: bool, cell: (u16, u16)) -> Result<Option<Fitted>, String> {
    let env = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    // The kitty graphics protocol is also spoken by other terminals; only kitty sets this.
    let kitty = protocol_kitty && env("KITTY_WINDOW_ID").is_some();
    let window_id = env("WINDOWID");
    let display = env("DISPLAY");
    if !kitty || tmux || window_id.is_none() || display.is_none() {
        return Ok(None);
    }
    let problem = |e: String| format!("could not fit the window: {e}");
    let window_id = window_id.unwrap_or_default();
    let state = run("xprop", &["-id", &window_id, "_NET_WM_STATE"]).map_err(problem)?;
    if !should_fit(
        kitty,
        tmux,
        Some(&window_id),
        display.as_deref(),
        Some(&state),
    ) {
        return Ok(None);
    }
    let original = geometry(&window_id).map_err(problem)?;
    let grid = crossterm::terminal::size().map_err(|e| problem(e.to_string()))?;
    let target = target_pixels(original, grid, cell, want_grid(cell));
    if target == original {
        return Ok(None);
    }
    let (w, h) = (target.0.to_string(), target.1.to_string());
    run("xdotool", &["windowsize", &window_id, &w, &h]).map_err(problem)?;
    // Not `windowsize --sync`: it waits for the size to change, forever if the window manager
    // keeps the old size.
    let end = Instant::now() + SETTLE;
    let mut fitted = geometry(&window_id).map_err(problem)?;
    while fitted == original && Instant::now() < end {
        thread::sleep(Duration::from_millis(50));
        fitted = geometry(&window_id).map_err(problem)?;
    }
    Ok(Some(Fitted {
        window_id,
        original,
        fitted,
    }))
}

/// Gives the window its original size back when dropped, if it still has the fitted size: a size
/// the user set by hand in the meantime is kept. It lives in `run`, so it drops on a normal quit,
/// on an error returned with `?`, and during the unwind after a panic.
pub struct Restore(pub Option<Fitted>);

impl Drop for Restore {
    fn drop(&mut self) {
        let Some(f) = &self.0 else {
            return;
        };
        // Errors are ignored: the terminal is already gone, there is nobody to tell.
        if geometry(&f.window_id).is_ok_and(|now| should_restore(now, f.fitted)) {
            let (w, h) = (f.original.0.to_string(), f.original.1.to_string());
            let _ = run("xdotool", &["windowsize", &f.window_id, &w, &h]);
        }
    }
}

/// The precondition for a fit. `wm_state` is `xprop`'s `_NET_WM_STATE` line; `None` means it is
/// unknown, and an unknown state is not fitted.
pub fn should_fit(
    kitty: bool,
    tmux: bool,
    window_id: Option<&str>,
    display: Option<&str>,
    wm_state: Option<&str>,
) -> bool {
    let Some(state) = wm_state else {
        return false;
    };
    kitty
        && !tmux
        && window_id.is_some_and(|w| !w.is_empty())
        && display.is_some_and(|d| !d.is_empty())
        && !state.contains("_NET_WM_STATE_MAXIMIZED")
        && !state.contains("_NET_WM_STATE_FULLSCREEN")
}

pub fn should_restore(current: (u32, u32), fitted: (u32, u32)) -> bool {
    current == fitted
}

/// The grid that holds the preview at the image's native size, its border, the panel and the
/// footer; at least `WIDE` columns, so the side-by-side layout stays.
pub fn want_grid((cw, ch): (u16, u16)) -> (u16, u16) {
    let (cw, ch) = (u32::from(cw.max(1)), u32::from(ch.max(1)));
    let cols = capture::WIDTH.div_ceil(cw) as u16 + 2 + PANEL_WIDTH;
    let rows = capture::HEIGHT.div_ceil(ch) as u16 + 2 + 1;
    (cols.max(WIDE), rows)
}

/// The window size that gives `want` cells. Kitty's padding and margins are the part of the
/// window that is not cells, `window - grid * cell`, and stay the same.
pub fn target_pixels(
    window: (u32, u32),
    grid: (u16, u16),
    (cw, ch): (u16, u16),
    want: (u16, u16),
) -> (u32, u32) {
    let (cw, ch) = (u32::from(cw), u32::from(ch));
    let pad_w = window.0.saturating_sub(u32::from(grid.0) * cw);
    let pad_h = window.1.saturating_sub(u32::from(grid.1) * ch);
    (
        u32::from(want.0) * cw + pad_w,
        u32::from(want.1) * ch + pad_h,
    )
}

/// `WIDTH` and `HEIGHT` from `xdotool getwindowgeometry --shell`.
pub fn parse_geometry(text: &str) -> Option<(u32, u32)> {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('=')?.trim().parse().ok())
    };
    Some((value("WIDTH")?, value("HEIGHT")?))
}

fn geometry(window_id: &str) -> Result<(u32, u32), String> {
    let text = run("xdotool", &["getwindowgeometry", "--shell", window_id])?;
    parse_geometry(&text).ok_or_else(|| format!("unexpected xdotool output: {text:?}"))
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{program} not found")
        } else {
            format!("{program}: {e}")
        }
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{program} failed: {}", err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_keeps_kittys_padding() {
        // Measured on Ivan's machine: a 1961x1249 window at 160x45 cells of 12x27 px.
        let want = want_grid((12, 27));
        assert_eq!(want, (134, 23), "80x20 preview + borders + panel + footer");
        assert_eq!(
            target_pixels((1961, 1249), (160, 45), (12, 27), want),
            (134 * 12 + 41, 23 * 27 + 34)
        );
    }

    #[test]
    fn a_narrow_font_still_gets_the_side_by_side_layout() {
        assert_eq!(want_grid((30, 60)).0, WIDE);
    }

    #[test]
    fn fits_only_kitty_itself_outside_tmux_and_not_maximized() {
        let normal = Some("_NET_WM_STATE(ATOM) = _NET_WM_STATE_FOCUSED");
        let absent = Some("_NET_WM_STATE:  not found.");
        let id = Some("130023435");
        let d = Some(":0");
        assert!(should_fit(true, false, id, d, normal));
        assert!(should_fit(true, false, id, d, absent));
        assert!(!should_fit(false, false, id, d, normal), "not kitty");
        assert!(!should_fit(true, true, id, d, normal), "tmux");
        assert!(!should_fit(true, false, None, d, normal), "no WINDOWID");
        assert!(
            !should_fit(true, false, Some(""), d, normal),
            "empty WINDOWID"
        );
        assert!(!should_fit(true, false, id, None, normal), "no DISPLAY");
        assert!(!should_fit(true, false, id, d, None), "state unknown");
        let max = Some(
            "_NET_WM_STATE(ATOM) = _NET_WM_STATE_MAXIMIZED_VERT, _NET_WM_STATE_MAXIMIZED_HORZ",
        );
        assert!(!should_fit(true, false, id, d, max), "maximized");
        let full = Some("_NET_WM_STATE(ATOM) = _NET_WM_STATE_FULLSCREEN");
        assert!(!should_fit(true, false, id, d, full), "fullscreen");
    }

    #[test]
    fn restores_only_an_unchanged_window() {
        assert!(should_restore((1649, 655), (1649, 655)));
        assert!(!should_restore((1920, 1080), (1649, 655)));
    }

    #[test]
    fn parses_xdotool_shell_geometry() {
        let text = "WINDOW=130023435\nX=4\nY=63\nWIDTH=1961\nHEIGHT=1249\nSCREEN=0\n";
        assert_eq!(parse_geometry(text), Some((1961, 1249)));
        assert_eq!(parse_geometry("WINDOW=1\nX=4\n"), None);
    }
}
