//! Terminal setup and restore. Restore runs on every exit path: the guard's `Drop`, and a panic
//! hook for panics on the UI thread.

use std::io::{self, Write};

use crossterm::{
    cursor::Show,
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

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

/// Writes what undoes [`Guard::enter`] on the screen: kitty images, alternate screen, cursor.
pub fn restore(out: &mut impl Write, kitty: bool) -> io::Result<()> {
    if kitty {
        out.write_all(KITTY_DELETE_ALL.as_bytes())?;
    }
    execute!(out, LeaveAlternateScreen, Show)?;
    out.flush()
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

    #[test]
    fn restore_leaves_the_alternate_screen_and_shows_the_cursor() {
        let mut out = Vec::new();
        restore(&mut out, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\x1b[?1049l"),
            "leave alternate screen: {text:?}"
        );
        assert!(text.contains("\x1b[?25h"), "show cursor: {text:?}");
        assert!(!text.contains(KITTY_DELETE_ALL));
    }

    #[test]
    fn restore_deletes_kitty_images_first() {
        let mut out = Vec::new();
        restore(&mut out, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(KITTY_DELETE_ALL));
    }
}
