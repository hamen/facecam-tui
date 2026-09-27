mod app;
mod camera;
mod capture;
mod control;
mod term;
mod ui;
mod usb;
mod v4l2;

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event as TermEvent};
use image::DynamicImage;
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui_image::{
    picker::{Capability, Picker, ProtocolType, cap_parser::QueryStdioOptions},
    protocol::{StatefulProtocol, StatefulProtocolType, kitty::StatefulKitty},
};

use crate::{
    app::App,
    capture::Capture,
    control::{Event, Shared},
};

/// Frames alternate between two kitty image ids, so the store holds at most two images.
///
/// Two, not one: the id is also the foreground colour of every placeholder cell, so alternating
/// changes every cell and ratatui rewrites them all each frame. With a single id only the first
/// cell (which carries the transmission) changes, and kitty then shows a large frame's first row
/// of cells and nothing below it (reproduced with a 1200x675 RGBA frame). It also keeps the frame
/// on screen intact while the next one is still being transmitted. [`term::draw_synced`] sends the
/// transmission and the rewritten cells as one synchronized update, so kitty never shows a mix of
/// the two frames.
const KITTY_IMAGE_IDS: [u32; 2] = [0x00fa_ce01, 0x00fa_ce02];

fn kitty_image_id(seq: u64) -> u32 {
    KITTY_IMAGE_IDS[(seq % 2) as usize]
}
/// Lets each frame through once. Every capture numbers its frames from 1, so the gate is reset
/// when a capture starts or stops; otherwise a new capture's frame with the old capture's last
/// number would be dropped.
#[derive(Default)]
struct FrameGate {
    last_seq: u64,
}

impl FrameGate {
    fn accept(&mut self, seq: u64) -> bool {
        if seq == self.last_seq {
            return false;
        }
        self.last_seq = seq;
        true
    }

    fn reset(&mut self) {
        self.last_seq = 0;
    }
}

/// The UI wakes at least this often while the preview runs, so new frames show without keys.
const FRAME_WAKE: Duration = Duration::from_millis(66);
const IDLE_WAKE: Duration = Duration::from_millis(250);
/// Three 500 ms USB timeouts.
const QUIT_WAIT: Duration = Duration::from_millis(1500);

fn main() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("facecam-tui needs a terminal (stdin and stdout must be a TTY)");
    }

    // The graphics query runs before raw mode, bounded to 1 s. Every terminal emulator answers
    // it (at least the DA1 part) within milliseconds, and then half-blocks are the fallback when
    // no graphics protocol is found. A terminal that never answers is not usable: ratatui-image
    // leaves its query thread reading stdin after the timeout, where it swallows keys and later
    // turns raw mode off under the running UI. So that case is an error, not a fallback.
    let options = QueryStdioOptions {
        timeout: Duration::from_secs(1),
        kitty_compression: true,
        ..QueryStdioOptions::default()
    };
    let picker = Picker::from_query_stdio_with_options(options).context(
        "the terminal did not answer the capability query; run facecam-tui in a terminal emulator",
    )?;
    let kitty = picker.protocol_type() == ProtocolType::Kitty;

    let guard = term::Guard::enter(kitty).context("could not set up the terminal")?;
    let result = run(&picker, kitty);
    drop(guard);
    result
}

fn run(picker: &Picker, kitty: bool) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let shared = Shared::default();
    let (events_tx, events) = mpsc::channel();
    let (sysfs, dev) = control::system_roots();
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("control".into())
            .spawn(move || control::run(shared, events_tx, sysfs, dev))?;
    }

    let cell = picker.font_size();
    let mut app = App::default();
    let mut capture: Option<(PathBuf, Capture)> = None;
    let mut protocol: Option<StatefulProtocol> = None;
    let mut frames = FrameGate::default();

    loop {
        let now = Instant::now();
        while let Ok(event) = events.try_recv() {
            match &event {
                Event::Up { video_node, .. } => {
                    let restart = capture.as_ref().is_none_or(|(p, _)| p != video_node);
                    if restart {
                        if let Some((_, old)) = capture.take() {
                            old.stop(Duration::from_millis(500));
                        }
                        capture = Some((
                            video_node.clone(),
                            Capture::start(video_node.clone(), shared.clone()),
                        ));
                        frames.reset();
                    }
                }
                Event::Gone { .. } => {
                    if let Some((_, old)) = capture.take() {
                        old.stop(Duration::from_millis(500));
                    }
                    frames.reset();
                    clear_preview(&mut protocol, kitty)?;
                }
                _ => {}
            }
            app.on_event(event);
        }
        while let Some(status) = capture.as_ref().and_then(|(_, c)| c.try_status()) {
            let streaming = status == capture::Status::Streaming;
            app.on_capture(status);
            if !streaming {
                clear_preview(&mut protocol, kitty)?;
            }
        }
        if let Some(frame) = capture.as_ref().and_then(|(_, c)| c.take_frame())
            && frames.accept(frame.seq)
        {
            protocol = Some(new_protocol(picker, frame.image, kitty_image_id(frame.seq)));
        }

        term::draw_synced(&mut terminal, |f| {
            ui::draw(
                f,
                &app,
                protocol.as_mut(),
                !kitty && picker.protocol_type() == ProtocolType::Halfblocks,
                (cell.width, cell.height),
            )
        })?;

        let wake = if capture.is_some() {
            FRAME_WAKE
        } else {
            IDLE_WAKE
        };
        let timeout = app.poll_timeout(now).map_or(wake, |t| t.min(wake));
        let mut commands = Vec::new();
        // Wait for the first event, then take every event already queued, so a burst of keys
        // (key repeat) is handled in one pass instead of one per frame.
        let mut wait = timeout;
        while event::poll(wait)? {
            wait = Duration::ZERO;
            match event::read()? {
                TermEvent::Key(key) => commands.extend(app.handle_key(key, Instant::now())),
                TermEvent::Resize(..) => {
                    if kitty {
                        delete_kitty_images()?;
                    }
                    terminal.autoresize()?;
                }
                _ => {}
            }
            if app.quit {
                break;
            }
        }
        commands.extend(app.tick(Instant::now()));
        if !commands.is_empty() {
            shared.submit(commands);
        }
        if app.quit {
            break;
        }
    }

    // Quit: stop the preview, then give the worker a bounded time to flush the last write.
    if let Some((_, capture)) = capture.take() {
        capture.stop(Duration::from_millis(500));
    }
    let end = Instant::now() + QUIT_WAIT;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(Event::QuitDone) | Err(_) => break,
            Ok(_) => {}
        }
    }
    Ok(())
}

fn new_protocol(picker: &Picker, image: DynamicImage, kitty_id: u32) -> StatefulProtocol {
    if picker.protocol_type() != ProtocolType::Kitty {
        return picker.new_resize_protocol(image);
    }
    let compress = picker
        .capabilities()
        .contains(&Capability::KittyCompression);
    let kitty = StatefulKitty::new(kitty_id, picker.tmux_detected(), compress);
    StatefulProtocol::new(
        image,
        picker.font_size(),
        None,
        StatefulProtocolType::Kitty(kitty),
    )
}

fn clear_preview(protocol: &mut Option<StatefulProtocol>, kitty: bool) -> Result<()> {
    *protocol = None;
    if kitty {
        delete_kitty_images()?;
    }
    Ok(())
}

fn delete_kitty_images() -> Result<()> {
    let mut out = io::stdout();
    out.write_all(term::KITTY_DELETE_ALL.as_bytes())?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_gate_lets_a_new_capture_start_from_one() {
        let mut gate = FrameGate::default();
        assert!(gate.accept(1));
        assert!(!gate.accept(1), "the same frame twice");
        gate.reset();
        assert!(gate.accept(1), "a new capture's first frame");
    }

    #[test]
    fn consecutive_frames_use_different_kitty_ids() {
        for seq in 1..6 {
            let (a, b) = (kitty_image_id(seq), kitty_image_id(seq + 1));
            assert_ne!(a, b, "seq {seq}");
            // The low 24 bits are the placeholder cells' colour: they must differ too, or ratatui
            // sees unchanged cells and skips them.
            assert_ne!(a & 0x00ff_ffff, b & 0x00ff_ffff, "seq {seq}");
        }
        assert_eq!(
            kitty_image_id(1),
            kitty_image_id(3),
            "only two ids in the store"
        );
    }
}
