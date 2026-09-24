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
    capture::{Capture, Slot},
    control::{Event, Shared},
};

/// One kitty image id for every frame: a new frame replaces the old image instead of piling up
/// in the terminal's image store.
const KITTY_IMAGE_ID: u32 = 0x00fa_ce01;
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

    let mut app = App::default();
    let slot: Slot = Slot::default();
    let (status_tx, statuses) = mpsc::channel();
    let mut capture: Option<(PathBuf, Capture)> = None;
    let mut protocol: Option<StatefulProtocol> = None;
    let mut last_seq = 0;

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
                            Capture::start(
                                video_node.clone(),
                                slot.clone(),
                                status_tx.clone(),
                                shared.clone(),
                            ),
                        ));
                    }
                }
                Event::Gone { .. } => {
                    if let Some((_, old)) = capture.take() {
                        old.stop(Duration::from_millis(500));
                    }
                    clear_preview(&mut protocol, &slot, kitty)?;
                }
                _ => {}
            }
            app.on_event(event);
        }
        while let Ok(status) = statuses.try_recv() {
            let streaming = status == capture::Status::Streaming;
            app.on_capture(status);
            if !streaming {
                clear_preview(&mut protocol, &slot, kitty)?;
            }
        }
        let frame = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(frame) = frame
            && frame.seq != last_seq
        {
            last_seq = frame.seq;
            protocol = Some(new_protocol(picker, frame.image));
        }

        terminal.draw(|f| {
            ui::draw(
                f,
                &app,
                protocol.as_mut(),
                !kitty && picker.protocol_type() == ProtocolType::Halfblocks,
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

fn new_protocol(picker: &Picker, image: DynamicImage) -> StatefulProtocol {
    if picker.protocol_type() != ProtocolType::Kitty {
        return picker.new_resize_protocol(image);
    }
    let compress = picker
        .capabilities()
        .contains(&Capability::KittyCompression);
    let kitty = StatefulKitty::new(KITTY_IMAGE_ID, picker.tmux_detected(), compress);
    StatefulProtocol::new(
        image,
        picker.font_size(),
        None,
        StatefulProtocolType::Kitty(kitty),
    )
}

fn clear_preview(protocol: &mut Option<StatefulProtocol>, slot: &Slot, kitty: bool) -> Result<()> {
    *protocol = None;
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
