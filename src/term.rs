//! Terminal setup and restore. Restore runs on every exit path: the guard's `Drop`, and a panic
//! hook for panics on the UI thread.

use std::io::{self, Write};

use crossterm::{
    cursor::Show,
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};

/// Deletes every kitty image placement and frees its data (`d=A`), quietly (`q=2`).
pub const KITTY_DELETE_ALL: &str = "\x1b_Ga=d,d=A,q=2\x1b\\";

pub struct Guard {
    kitty: bool,
}

impl Guard {
    pub fn enter(kitty: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self { kitty };
        execute!(io::stdout(), EnterAlternateScreen)?;
        install_panic_hook(kitty);
        Ok(guard)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = restore(&mut io::stdout(), self.kitty);
        let _ = disable_raw_mode();
    }
}

/// Draws one frame as a synchronized update (DECSET 2026): the terminal shows it only when all of
/// it has arrived. Without it kitty can render between the image transmission and the rewrite of
/// the placeholder cells, and shows the new frame's rows above the old frame's rows, which flickers
/// as soon as something moves. The End is written even when the draw fails, so the terminal never
/// holds its screen waiting for it.
pub fn draw_synced<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut Frame),
) -> io::Result<()> {
    if let Err(err) = execute!(terminal.backend_mut(), BeginSynchronizedUpdate) {
        let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
        return Err(err);
    }
    let drawn = terminal.draw(render).map(|_| ());
    let ended = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn.and(ended)
}

/// Writes what undoes [`Guard::enter`] on the screen: an open synchronized update, kitty images,
/// alternate screen, cursor. The End comes first: a panic inside [`draw_synced`] reaches the panic
/// hook with the update still open, and the terminal would hold everything after it. Every step
/// runs even when an earlier one fails; the first error is returned.
pub fn restore(out: &mut impl Write, kitty: bool) -> io::Result<()> {
    let ended = execute!(out, EndSynchronizedUpdate);
    let deleted = if kitty {
        out.write_all(KITTY_DELETE_ALL.as_bytes())
    } else {
        Ok(())
    };
    let left = execute!(out, LeaveAlternateScreen, Show);
    ended.and(deleted).and(left).and(out.flush())
}

fn install_panic_hook(kitty: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only the UI thread owns the terminal. A panic in a worker thread must not tear the
        // screen down under a UI that is still running.
        if std::thread::current().name() == Some("main") {
            let _ = restore(&mut io::stdout(), kitty);
            let _ = disable_raw_mode();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use ratatui::{TerminalOptions, Viewport, layout::Rect, widgets::Paragraph};

    const SYNC_BEGIN: &str = "\x1b[?2026h";
    const SYNC_END: &str = "\x1b[?2026l";

    /// Which step the [`Recorder`] fails. Each failure has its own message, so a test can tell
    /// which error came back.
    #[derive(Clone, Copy, Default, PartialEq)]
    enum Fail {
        #[default]
        Nothing,
        /// Every write that is not a synchronized-update sequence: Begin and End get through,
        /// the frame does not.
        Frame,
        /// The flush right after the Begin write: the update is open, but Begin reports an error.
        BeginFlush,
        /// The End write.
        End,
    }

    /// Records everything written to it, and fails the step named by `fail`.
    #[derive(Clone, Default)]
    struct Recorder {
        out: Rc<RefCell<Vec<u8>>>,
        fail: Fail,
        after_begin: Rc<Cell<bool>>,
    }

    impl Recorder {
        fn failing(fail: Fail) -> Self {
            Self {
                fail,
                ..Self::default()
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.out.borrow().clone()).unwrap()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let begin = buf == SYNC_BEGIN.as_bytes();
            let end = buf == SYNC_END.as_bytes();
            if self.fail == Fail::Frame && !begin && !end {
                return Err(io::Error::other("frame write failed"));
            }
            if self.fail == Fail::End && end {
                return Err(io::Error::other("end write failed"));
            }
            self.after_begin.set(begin);
            self.out.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail == Fail::BeginFlush && self.after_begin.get() {
                return Err(io::Error::other("begin flush failed"));
            }
            Ok(())
        }
    }

    /// A terminal of a fixed size: `Terminal::new` would ask for the size, which needs a TTY.
    fn terminal(recorder: &Recorder) -> Terminal<CrosstermBackend<Recorder>> {
        let viewport = Viewport::Fixed(Rect::new(0, 0, 20, 2));
        Terminal::with_options(
            CrosstermBackend::new(recorder.clone()),
            TerminalOptions { viewport },
        )
        .unwrap()
    }

    fn marker(f: &mut Frame) {
        f.render_widget(Paragraph::new("frame-marker"), f.area());
    }

    #[test]
    fn a_frame_is_drawn_inside_one_synchronized_update() {
        let recorder = Recorder::default();
        // Read before the terminal drops: its `Drop` shows the cursor again.
        let mut terminal = terminal(&recorder);
        draw_synced(&mut terminal, marker).unwrap();
        let text = recorder.text();
        assert!(text.starts_with(SYNC_BEGIN), "begin first: {text:?}");
        assert!(text.ends_with(SYNC_END), "end last: {text:?}");
        assert!(text.contains("frame-marker"), "the frame: {text:?}");
    }

    #[test]
    fn a_failed_draw_still_ends_the_update_and_returns_the_draw_error() {
        let recorder = Recorder::failing(Fail::Frame);
        let mut terminal = terminal(&recorder);
        let err = draw_synced(&mut terminal, marker).unwrap_err();
        assert_eq!(err.to_string(), "frame write failed");
        let text = recorder.text();
        assert!(text.starts_with(SYNC_BEGIN), "begin: {text:?}");
        assert!(text.ends_with(SYNC_END), "end after the failure: {text:?}");
        assert!(!text.contains("frame-marker"));
    }

    #[test]
    fn a_failed_begin_skips_the_draw_and_still_ends_the_update() {
        let recorder = Recorder::failing(Fail::BeginFlush);
        let mut terminal = terminal(&recorder);
        let mut rendered = false;
        let err = draw_synced(&mut terminal, |_| rendered = true).unwrap_err();
        assert_eq!(err.to_string(), "begin flush failed");
        assert!(!rendered, "no draw after a failed begin");
        assert_eq!(recorder.text(), format!("{SYNC_BEGIN}{SYNC_END}"));
    }

    #[test]
    fn a_failed_end_after_a_good_draw_is_returned() {
        let recorder = Recorder::failing(Fail::End);
        let mut terminal = terminal(&recorder);
        let err = draw_synced(&mut terminal, marker).unwrap_err();
        assert_eq!(err.to_string(), "end write failed");
        assert!(recorder.text().contains("frame-marker"));
    }

    #[test]
    fn restore_runs_every_step_when_the_end_fails() {
        let mut recorder = Recorder::failing(Fail::End);
        let err = restore(&mut recorder, true).unwrap_err();
        assert_eq!(err.to_string(), "end write failed");
        let text = recorder.text();
        assert!(text.starts_with(KITTY_DELETE_ALL), "{text:?}");
        assert!(
            text.contains("\x1b[?1049l"),
            "leave alternate screen: {text:?}"
        );
        assert!(text.contains("\x1b[?25h"), "show cursor: {text:?}");
    }

    #[test]
    fn restore_leaves_the_alternate_screen_and_shows_the_cursor() {
        let mut out = Vec::new();
        restore(&mut out, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(SYNC_END), "end an open update: {text:?}");
        assert!(
            text.contains("\x1b[?1049l"),
            "leave alternate screen: {text:?}"
        );
        assert!(text.contains("\x1b[?25h"), "show cursor: {text:?}");
        assert!(!text.contains(KITTY_DELETE_ALL));
    }

    #[test]
    fn restore_ends_the_sync_then_deletes_kitty_images() {
        let mut out = Vec::new();
        restore(&mut out, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        let end = text.find(SYNC_END).expect("sync end");
        let delete = text.find(KITTY_DELETE_ALL).expect("kitty delete");
        let leave = text.find("\x1b[?1049l").expect("leave alternate screen");
        assert_eq!(end, 0, "{text:?}");
        assert!(end < delete && delete < leave, "order: {text:?}");
    }
}
