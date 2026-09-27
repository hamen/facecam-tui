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
pub const SETTLE: Duration = Duration::from_millis(1000);

/// A window this process resized.
#[derive(Debug, PartialEq)]
pub struct Fitted {
    window_id: String,
    original: (u32, u32),
    /// The size read back after the resize, not the size asked for: a window manager may clamp it.
    fitted: (u32, u32),
}

/// What decides whether this is a window to fit.
#[derive(Debug, Default)]
pub struct Env {
    /// The terminal speaks the kitty graphics protocol (WezTerm and Ghostty do too).
    pub protocol_kitty: bool,
    /// `$KITTY_WINDOW_ID` is set: only kitty itself sets it.
    pub kitty_window_id: bool,
    pub tmux: bool,
    /// `$WINDOWID`.
    pub window_id: Option<String>,
    /// `$DISPLAY` is set.
    pub display: bool,
}

impl Env {
    pub fn from_process(protocol_kitty: bool, tmux: bool) -> Self {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Self {
            protocol_kitty,
            kitty_window_id: var("KITTY_WINDOW_ID").is_some(),
            tmux,
            window_id: var("WINDOWID"),
            display: var("DISPLAY").is_some(),
        }
    }

    /// The window to fit, when this is kitty itself on X11 outside tmux.
    fn window(&self) -> Option<&str> {
        let kitty = self.protocol_kitty && self.kitty_window_id;
        (kitty && !self.tmux && self.display)
            .then_some(self.window_id.as_deref())
            .flatten()
            .filter(|w| !w.is_empty())
    }
}

/// The X11 calls, behind a trait so the fit and the restore can run against a fake.
pub trait X11 {
    /// `xprop`'s `_NET_WM_STATE` line.
    fn wm_state(&mut self, window: &str) -> Result<String, String>;
    fn geometry(&mut self, window: &str) -> Result<(u32, u32), String>;
    fn resize(&mut self, window: &str, size: (u32, u32)) -> Result<(), String>;
}

/// `xprop` and `xdotool`.
pub struct Xdotool;

impl X11 for Xdotool {
    fn wm_state(&mut self, window: &str) -> Result<String, String> {
        run("xprop", &["-id", window, "_NET_WM_STATE"])
    }

    fn geometry(&mut self, window: &str) -> Result<(u32, u32), String> {
        let text = run("xdotool", &["getwindowgeometry", "--shell", window])?;
        parse_geometry(&text).ok_or_else(|| format!("unexpected xdotool output: {text:?}"))
    }

    fn resize(&mut self, window: &str, (w, h): (u32, u32)) -> Result<(), String> {
        run(
            "xdotool",
            &["windowsize", window, &w.to_string(), &h.to_string()],
        )
        .map(|_| ())
    }
}

/// Fits the window when `env` says it is kitty itself on X11 outside tmux, and the window is
/// neither maximized nor fullscreen. `grid` is the terminal size in cells. `Ok(None)`: nothing to
/// do. `Err`: a message for the panel, and the window was not resized.
pub fn fit(
    env: &Env,
    x: &mut impl X11,
    grid: (u16, u16),
    cell: (u16, u16),
    settle: Duration,
) -> Result<Option<Fitted>, String> {
    let Some(window) = env.window() else {
        return Ok(None);
    };
    let problem = |e: String| format!("could not fit the window: {e}");
    // An unknown state is not fitted: resizing a maximized window fights the window manager.
    let state = x.wm_state(window).map_err(problem)?;
    if !normal_state(&state) {
        return Ok(None);
    }
    let original = x.geometry(window).map_err(problem)?;
    let target = target_pixels(original, grid, cell, want_grid(cell));
    if target == original {
        return Ok(None);
    }
    x.resize(window, target).map_err(problem)?;
    // From here the window may have changed size, so a `Fitted` always comes back: the restore
    // must still run. Not `windowsize --sync`: it waits for the size to change, forever if the
    // window manager keeps the old size. When the size cannot be read back, the requested size
    // stands in for it.
    let end = Instant::now() + settle;
    let fitted = loop {
        match x.geometry(window) {
            Ok(size) if size != original || Instant::now() >= end => break size,
            Ok(_) => thread::sleep(Duration::from_millis(50)),
            Err(_) => break target,
        }
    };
    Ok(Some(Fitted {
        window_id: window.to_string(),
        original,
        fitted,
    }))
}

/// Gives the window its original size back when dropped (see [`restore`]). It lives in `run`, so
/// it drops on a normal quit, on an error returned with `?`, and during the unwind after a panic.
pub struct Restore(pub Option<Fitted>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            restore(f, &mut Xdotool);
        }
    }
}

/// Resizes the window back to its original size if it still has the fitted size: a size the user
/// set by hand in the meantime is kept. Errors are ignored: the terminal is already gone, there is
/// nobody to tell.
pub fn restore(f: &Fitted, x: &mut impl X11) {
    if x.geometry(&f.window_id)
        .is_ok_and(|now| should_restore(now, f.fitted))
    {
        let _ = x.resize(&f.window_id, f.original);
    }
}

/// `xprop`'s `_NET_WM_STATE` line for a window that is neither maximized nor fullscreen.
fn normal_state(state: &str) -> bool {
    !state.contains("_NET_WM_STATE_MAXIMIZED") && !state.contains("_NET_WM_STATE_FULLSCREEN")
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

    /// Records every call, in order, and answers from a script.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        state: Option<String>,
        /// Answers to successive geometry reads; past the end, the last one repeats.
        sizes: Vec<Result<(u32, u32), String>>,
        resize_fails: bool,
    }

    impl X11 for Fake {
        fn wm_state(&mut self, window: &str) -> Result<String, String> {
            self.calls.push(format!("state {window}"));
            self.state
                .clone()
                .ok_or_else(|| "xprop not found".to_string())
        }

        fn geometry(&mut self, window: &str) -> Result<(u32, u32), String> {
            self.calls.push(format!("geometry {window}"));
            if self.sizes.len() > 1 {
                self.sizes.remove(0)
            } else {
                self.sizes[0].clone()
            }
        }

        fn resize(&mut self, window: &str, (w, h): (u32, u32)) -> Result<(), String> {
            self.calls.push(format!("resize {window} {w}x{h}"));
            if self.resize_fails {
                return Err("xdotool failed".into());
            }
            Ok(())
        }
    }

    const NORMAL: &str = "_NET_WM_STATE(ATOM) = _NET_WM_STATE_FOCUSED";
    const ORIGINAL: (u32, u32) = (1961, 1249);
    const TARGET: (u32, u32) = (1649, 655);

    fn kitty() -> Env {
        Env {
            protocol_kitty: true,
            kitty_window_id: true,
            tmux: false,
            window_id: Some("42".into()),
            display: true,
        }
    }

    fn fake(sizes: Vec<Result<(u32, u32), String>>) -> Fake {
        Fake {
            state: Some(NORMAL.into()),
            sizes,
            ..Fake::default()
        }
    }

    fn fit_now(env: &Env, x: &mut Fake) -> Result<Option<Fitted>, String> {
        fit(env, x, (160, 45), (12, 27), Duration::ZERO)
    }

    #[test]
    fn fits_kitty_and_measures_the_new_size() {
        let mut x = fake(vec![Ok(ORIGINAL), Ok((1650, 656))]);
        let fitted = fit_now(&kitty(), &mut x).unwrap().unwrap();
        assert_eq!(
            x.calls,
            [
                "state 42",
                "geometry 42",
                "resize 42 1649x655",
                "geometry 42"
            ]
        );
        assert_eq!(fitted.original, ORIGINAL);
        assert_eq!(
            fitted.fitted,
            (1650, 656),
            "the size read back, not the target"
        );
    }

    #[test]
    fn a_failed_read_back_still_returns_a_fitted_window() {
        let mut x = fake(vec![Ok(ORIGINAL), Err("xdotool failed".into())]);
        let fitted = fit_now(&kitty(), &mut x).unwrap().unwrap();
        assert_eq!(fitted.fitted, TARGET, "the requested size stands in");
        assert_eq!(fitted.original, ORIGINAL);
    }

    #[test]
    fn leaves_other_terminals_tmux_and_missing_env_alone() {
        let cases = [
            Env {
                kitty_window_id: false,
                ..kitty()
            },
            Env {
                protocol_kitty: false,
                ..kitty()
            },
            Env {
                tmux: true,
                ..kitty()
            },
            Env {
                window_id: None,
                ..kitty()
            },
            Env {
                window_id: Some(String::new()),
                ..kitty()
            },
            Env {
                display: false,
                ..kitty()
            },
        ];
        for env in cases {
            let mut x = fake(vec![Ok(ORIGINAL)]);
            assert_eq!(fit_now(&env, &mut x), Ok(None), "{env:?}");
            assert!(x.calls.is_empty(), "{env:?}: {:?}", x.calls);
        }
    }

    #[test]
    fn leaves_a_maximized_or_fullscreen_window_alone() {
        for state in [
            "_NET_WM_STATE(ATOM) = _NET_WM_STATE_MAXIMIZED_VERT, _NET_WM_STATE_MAXIMIZED_HORZ",
            "_NET_WM_STATE(ATOM) = _NET_WM_STATE_FULLSCREEN",
        ] {
            let mut x = Fake {
                state: Some(state.into()),
                ..fake(vec![Ok(ORIGINAL)])
            };
            assert_eq!(fit_now(&kitty(), &mut x), Ok(None), "{state}");
            assert_eq!(x.calls, ["state 42"]);
        }
        let mut x = fake(vec![Ok(ORIGINAL)]);
        x.state = Some("_NET_WM_STATE:  not found.".into());
        assert!(
            fit_now(&kitty(), &mut x).unwrap().is_some(),
            "no state is normal"
        );
    }

    #[test]
    fn an_unknown_state_or_a_failed_resize_is_a_problem_without_a_fit() {
        let mut x = Fake {
            state: None,
            ..fake(vec![Ok(ORIGINAL)])
        };
        let err = fit_now(&kitty(), &mut x).unwrap_err();
        assert_eq!(err, "could not fit the window: xprop not found");
        assert_eq!(x.calls, ["state 42"], "no resize");

        let mut x = Fake {
            resize_fails: true,
            ..fake(vec![Ok(ORIGINAL)])
        };
        assert!(fit_now(&kitty(), &mut x).is_err());
    }

    #[test]
    fn restore_resizes_back_only_an_unchanged_window() {
        let f = Fitted {
            window_id: "42".into(),
            original: ORIGINAL,
            fitted: TARGET,
        };
        let mut x = fake(vec![Ok(TARGET)]);
        restore(&f, &mut x);
        assert_eq!(x.calls, ["geometry 42", "resize 42 1961x1249"]);

        let mut x = fake(vec![Ok((1920, 1080))]);
        restore(&f, &mut x);
        assert_eq!(x.calls, ["geometry 42"], "resized by hand: kept");

        let mut x = fake(vec![Err("gone".into())]);
        restore(&f, &mut x);
        assert_eq!(x.calls, ["geometry 42"], "unreadable: left alone");
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
