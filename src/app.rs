//! Pure UI state: focus, values, number entry, pending writes, throttle. No I/O.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    camera::Mode,
    capture,
    control::{Command, Event, Target},
};

/// At most one exposure write per window; the last value is always sent.
pub const THROTTLE: Duration = Duration::from_millis(30);
/// The bar spans 1..=BAR_MAX: the 30 fps ceiling (1/30 s = 333 × 100 µs).
pub const BAR_MAX: u32 = 333;
/// Above this, apps capturing at 60 fps drop frames (1/60 s = 166 × 100 µs).
pub const FPS60_MAX: u32 = 166;

const EXPOSURE_UNKNOWN: &str = "exposure value unknown — press r to reload";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Exposure,
    Brightness,
    Mode,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Exposure => Focus::Brightness,
            Focus::Brightness => Focus::Mode,
            Focus::Mode => Focus::Exposure,
        }
    }

    fn prev(self) -> Self {
        match self {
            Focus::Exposure => Focus::Mode,
            Focus::Brightness => Focus::Exposure,
            Focus::Mode => Focus::Brightness,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    NoCamera,
    Starting,
    Streaming,
    Busy,
    Problem(String),
}

#[derive(Debug)]
pub struct App {
    pub focus: Focus,
    pub exposure: Option<u32>,
    pub exposure_range: Result<(u32, u32), String>,
    pub brightness: Option<i64>,
    pub brightness_range: Option<(i64, i64)>,
    pub mode: Option<Mode>,
    /// Digits typed in number entry, when open.
    pub entry: Option<String>,
    pub message: Option<String>,
    /// Why the window could not be fitted at startup. Kept apart from `message`, which a camera
    /// event clears; this one goes on the next key press.
    pub window_problem: Option<String>,
    pub device: Option<String>,
    pub preview: Preview,
    pub quit: bool,
    generation: u64,
    rev: u64,
    /// Newest write sent to the worker, per control (index = Target as usize).
    last_write: [u64; 3],
    /// Exposure changes held back by the throttle.
    held: Option<(u64, u32, bool)>,
    last_exposure_send: Option<Instant>,
}

fn idx(target: Target) -> usize {
    match target {
        Target::Exposure => 0,
        Target::Brightness => 1,
        Target::Mode => 2,
    }
}

impl Default for App {
    fn default() -> Self {
        Self {
            focus: Focus::Exposure,
            exposure: None,
            exposure_range: Err("camera not connected".into()),
            brightness: None,
            brightness_range: None,
            mode: None,
            entry: None,
            message: None,
            window_problem: None,
            device: None,
            preview: Preview::NoCamera,
            quit: false,
            generation: 0,
            rev: 0,
            last_write: [0; 3],
            held: None,
            last_exposure_send: None,
        }
    }
}

impl App {
    fn next_rev(&mut self) -> u64 {
        self.rev += 1;
        self.rev
    }

    /// How long the event loop may sleep before [`App::tick`] has something to send.
    pub fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        self.held?;
        let sent = self.last_exposure_send?;
        Some(THROTTLE.saturating_sub(now.saturating_duration_since(sent)))
    }

    /// Sends a held exposure write once its window has passed.
    pub fn tick(&mut self, now: Instant) -> Vec<Command> {
        match (self.held, self.last_exposure_send) {
            (Some(_), Some(sent)) if now.saturating_duration_since(sent) >= THROTTLE => {
                self.flush_exposure(now)
            }
            _ => Vec::new(),
        }
    }

    fn flush_exposure(&mut self, now: Instant) -> Vec<Command> {
        let Some((rev, value, ensure_shutter)) = self.held.take() else {
            return Vec::new();
        };
        self.last_exposure_send = Some(now);
        vec![Command::Exposure {
            rev,
            value,
            ensure_shutter,
        }]
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Command> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        self.window_problem = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.request_quit(now);
        }
        // Other Ctrl and Alt chords are not bindings: Ctrl+A must not toggle the mode.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        if self.entry.is_some() {
            return self.entry_key(key, now);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(now),
            KeyCode::Tab => {
                self.focus = self.focus.next();
                Vec::new()
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = self.focus.prev();
                Vec::new()
            }
            KeyCode::Down => {
                self.focus = self.focus.next();
                Vec::new()
            }
            KeyCode::Left => self.arrow(false, shift, now),
            KeyCode::Right => self.arrow(true, shift, now),
            KeyCode::PageDown => self.step(-100, now),
            KeyCode::PageUp => self.step(100, now),
            // Exposure keys: on another control they would change a value that has no focus.
            KeyCode::Char('[') if self.focus == Focus::Exposure => self.snap(false, now),
            KeyCode::Char(']') if self.focus == Focus::Exposure => self.snap(true, now),
            KeyCode::Char('a') => self.toggle_mode(),
            KeyCode::Char('r') => self.read_all(),
            KeyCode::Enter | KeyCode::Char(':') if self.focus != Focus::Mode => {
                self.entry = Some(String::new());
                self.message = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn entry_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Command> {
        let entry = self.entry.get_or_insert_with(String::new);
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if entry.len() < 10 {
                    entry.push(c);
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                entry.pop();
                Vec::new()
            }
            KeyCode::Esc => {
                self.entry = None;
                Vec::new()
            }
            KeyCode::Enter => {
                let text = self.entry.take().unwrap_or_default();
                // An empty entry closes like Esc: it is not an out-of-range value.
                if text.is_empty() {
                    return Vec::new();
                }
                self.apply_typed(&text, now)
            }
            _ => Vec::new(),
        }
    }

    fn apply_typed(&mut self, text: &str, now: Instant) -> Vec<Command> {
        let parsed: Option<i64> = text.parse().ok();
        match self.focus {
            Focus::Exposure => {
                let Ok((min, max)) = self.exposure_range.clone() else {
                    return self.refuse_exposure();
                };
                match parsed
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| (min..=max).contains(v))
                {
                    Some(v) => self.set_exposure(v, now),
                    None => self.error(format!("exposure must be {min}..{max}")),
                }
            }
            Focus::Brightness => {
                let Some((min, max)) = self.brightness_range else {
                    return self.error("brightness range unknown".into());
                };
                match parsed.filter(|v| (min..=max).contains(v)) {
                    Some(v) => self.set_brightness(v),
                    None => self.error(format!("brightness must be {min}..{max}")),
                }
            }
            Focus::Mode => Vec::new(),
        }
    }

    fn error(&mut self, message: String) -> Vec<Command> {
        self.message = Some(message);
        Vec::new()
    }

    fn refuse_exposure(&mut self) -> Vec<Command> {
        let reason = self.exposure_range.clone().err().unwrap_or_default();
        self.error(format!("exposure unavailable: {reason}"))
    }

    /// `←` / `→`. Exposure jumps between flicker-free values (as `[` `]`), Shift steps by 10;
    /// Brightness steps by 10, Shift by 1; Mode toggles.
    fn arrow(&mut self, up: bool, shift: bool, now: Instant) -> Vec<Command> {
        let sign = if up { 1 } else { -1 };
        match (self.focus, shift) {
            (Focus::Exposure, false) => self.snap(up, now),
            (Focus::Exposure, true) | (Focus::Brightness, false) => self.step(10 * sign, now),
            (Focus::Brightness, true) => self.step(sign, now),
            (Focus::Mode, _) => self.toggle_mode(),
        }
    }

    fn step(&mut self, delta: i64, now: Instant) -> Vec<Command> {
        match self.focus {
            Focus::Exposure => {
                let Ok((min, max)) = self.exposure_range.clone() else {
                    return self.refuse_exposure();
                };
                let Some(current) = self.exposure else {
                    return self.error(EXPOSURE_UNKNOWN.into());
                };
                let v = (i64::from(current) + delta).clamp(i64::from(min), i64::from(max));
                self.set_exposure(v as u32, now)
            }
            Focus::Brightness => {
                let Some((min, max)) = self.brightness_range else {
                    return self.error("brightness range unknown".into());
                };
                let Some(current) = self.brightness else {
                    return self.error("brightness value unknown — press r to reload".into());
                };
                self.set_brightness((current + delta).clamp(min, max))
            }
            // The arrows toggle the mode (see `arrow`); PgUp / PgDn do nothing here.
            Focus::Mode => Vec::new(),
        }
    }

    fn snap(&mut self, up: bool, now: Instant) -> Vec<Command> {
        let Ok((min, max)) = self.exposure_range.clone() else {
            return self.refuse_exposure();
        };
        let Some(current) = self.exposure else {
            return self.error(EXPOSURE_UNKNOWN.into());
        };
        // `snap` stays within 100..=max; a device range without a multiple of 100 in that
        // direction would get a value it refuses.
        let target = snap(current, max, up);
        if !(min..=max).contains(&target) {
            return self.error(format!("no flicker-free value in {min}..{max}"));
        }
        self.set_exposure(target, now)
    }

    fn set_exposure(&mut self, value: u32, now: Instant) -> Vec<Command> {
        let ensure = self.mode != Some(Mode::Shutter);
        let rev = self.next_rev();
        self.exposure = Some(value);
        self.mode = Some(Mode::Shutter);
        self.last_write[idx(Target::Exposure)] = rev;
        let ensure = ensure || self.held.is_some_and(|(_, _, e)| e);
        self.held = Some((rev, value, ensure));
        match self.last_exposure_send {
            Some(sent) if now.saturating_duration_since(sent) < THROTTLE => Vec::new(),
            _ => self.flush_exposure(now),
        }
    }

    /// Revisions must leave in order: a held exposure (older revision) goes out before any newer
    /// write, instead of after it from `tick`. The worker only orders within one batch.
    fn release_held(&mut self) -> Vec<Command> {
        self.flush_exposure(Instant::now())
    }

    fn set_brightness(&mut self, value: i64) -> Vec<Command> {
        let mut commands = self.release_held();
        let rev = self.next_rev();
        self.brightness = Some(value);
        self.last_write[idx(Target::Brightness)] = rev;
        commands.push(Command::Brightness { rev, value });
        commands
    }

    fn toggle_mode(&mut self) -> Vec<Command> {
        let Some(mode) = self.mode else {
            return self.error("mode unknown — press r to reload".into());
        };
        let mode = mode.toggled();
        // Auto cancels a held exposure; Shutter Priority sends it first.
        let mut commands = match mode {
            Mode::Auto => {
                self.held = None;
                Vec::new()
            }
            Mode::Shutter => self.release_held(),
        };
        let rev = self.next_rev();
        self.mode = Some(mode);
        self.last_write[idx(Target::Mode)] = rev;
        commands.push(Command::Mode { rev, mode });
        commands
    }

    fn read_all(&mut self) -> Vec<Command> {
        self.held = None;
        let rev = self.next_rev();
        self.message = None;
        vec![Command::ReadAll { rev }]
    }

    fn request_quit(&mut self, now: Instant) -> Vec<Command> {
        self.entry = None;
        self.quit = true;
        let mut commands = self.flush_exposure(now);
        let rev = self.next_rev();
        commands.push(Command::Quit { rev });
        commands
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Up {
                generation,
                video_node,
                ranges,
            } => {
                self.generation = generation;
                self.device = Some(video_node.display().to_string());
                self.message = match (&ranges.exposure, &ranges.brightness) {
                    (Err(e), _) => Some(format!("exposure unavailable: {e}")),
                    (_, Err(e)) => Some(format!("brightness unavailable: {e}")),
                    _ => None,
                };
                self.exposure_range = ranges.exposure;
                self.brightness_range = ranges.brightness.ok();
            }
            Event::Gone { generation, reason } => {
                if generation < self.generation {
                    return;
                }
                self.device = None;
                self.held = None;
                // Writes that never reached the device are gone with it; without this reset a
                // read-back after the reconnect would look older than them and be ignored.
                self.last_write = [0; 3];
                self.brightness_range = None;
                self.exposure = None;
                self.brightness = None;
                self.mode = None;
                self.exposure_range = Err(reason.clone());
                self.preview = Preview::NoCamera;
                self.message = Some(reason);
            }
            Event::Values {
                generation,
                rev,
                values,
            } => {
                if generation != self.generation {
                    return;
                }
                if rev >= self.last_write[idx(Target::Exposure)] && self.held.is_none() {
                    match values.exposure {
                        Ok(v) => self.exposure = Some(v),
                        Err(e) => {
                            self.exposure = None;
                            self.message = Some(e);
                        }
                    }
                }
                if rev >= self.last_write[idx(Target::Brightness)] {
                    match values.brightness {
                        Ok(v) => self.brightness = Some(v),
                        Err(e) => {
                            self.brightness = None;
                            self.message = Some(e);
                        }
                    }
                }
                if rev >= self.last_write[idx(Target::Mode)] {
                    match values.mode {
                        Ok(m) => self.mode = Some(m),
                        Err(e) => {
                            self.mode = None;
                            self.message = Some(e);
                        }
                    }
                }
            }
            Event::Failed {
                generation,
                rev,
                target,
                error,
            } => {
                if generation != self.generation {
                    return;
                }
                if target == Target::Exposure && self.held.is_some_and(|(r, _, _)| r <= rev) {
                    self.held = None;
                }
                self.message = Some(error);
            }
            Event::QuitDone => {}
        }
    }

    pub fn on_capture(&mut self, status: capture::Status) {
        self.preview = match status {
            capture::Status::Starting => Preview::Starting,
            capture::Status::Streaming => Preview::Streaming,
            capture::Status::Busy => Preview::Busy,
            capture::Status::NoAccess(e) => Preview::Problem(format!("no access: {e}")),
            capture::Status::Gone(e) | capture::Status::Error(e) => Preview::Problem(e),
        };
    }
}

/// `[` / `]`: previous / next multiple of 100 within 100..=max (flicker-free under 50 Hz).
/// Below 100, `[` goes UP to 100, the lowest flicker-free value.
pub fn snap(value: u32, max: u32, up: bool) -> u32 {
    let top = (max / 100 * 100).max(100);
    let v = if up {
        (value / 100 + 1) * 100
    } else if value <= 100 {
        100
    } else {
        (value - 1) / 100 * 100
    };
    v.clamp(100, top)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::control::{Ranges, Values};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// A connected camera in Shutter Priority at exposure 200, brightness 40.
    fn app() -> App {
        let mut app = App::default();
        app.on_event(Event::Up {
            generation: 1,
            video_node: PathBuf::from("/dev/video0"),
            ranges: Ranges {
                exposure: Ok((1, 2500)),
                brightness: Ok((0, 255)),
            },
        });
        app.on_event(Event::Values {
            generation: 1,
            rev: 0,
            values: Values {
                exposure: Ok(200),
                mode: Ok(Mode::Shutter),
                brightness: Ok(40),
            },
        });
        app
    }

    fn exposure_writes(commands: &[Command]) -> Vec<u32> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::Exposure { value, .. } => Some(*value),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn snapping() {
        assert_eq!((snap(150, 2500, false), snap(150, 2500, true)), (100, 200));
        assert_eq!((snap(200, 2500, false), snap(200, 2500, true)), (100, 300));
        assert_eq!((snap(50, 2500, false), snap(50, 2500, true)), (100, 100));
        assert_eq!(
            (snap(2500, 2500, false), snap(2500, 2500, true)),
            (2400, 2500)
        );
        assert_eq!(snap(1, 2500, false), 100);
        assert_eq!(snap(640, 650, true), 600, "top comes from the device max");
    }

    #[test]
    fn ctrl_and_alt_chords_do_nothing() {
        let mut a = app();
        let now = Instant::now();
        for k in [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
        ] {
            assert!(a.handle_key(k, now).is_empty(), "{k:?}");
        }
        assert!(!a.quit);
        assert_eq!((a.mode, a.exposure), (Some(Mode::Shutter), Some(200)));

        // Inside the entry too; Ctrl+C still quits.
        a.handle_key(key(KeyCode::Enter), now);
        a.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT), now);
        assert_eq!(a.entry.as_deref(), Some(""));
        a.handle_key(ctrl_c(), now);
        assert!(a.quit);
    }

    #[test]
    fn enter_on_an_empty_entry_closes_it_quietly() {
        let now = Instant::now();
        for focus in [Focus::Exposure, Focus::Brightness] {
            let mut a = app();
            a.focus = focus;
            a.handle_key(key(KeyCode::Enter), now);
            assert!(a.handle_key(key(KeyCode::Enter), now).is_empty());
            assert_eq!((a.entry.as_deref(), a.message.as_deref()), (None, None));
        }
    }

    #[test]
    fn snap_keys_act_only_on_exposure_focus() {
        let now = Instant::now();
        let mut a = app();
        a.focus = Focus::Brightness;
        for k in ['[', ']'] {
            assert!(a.handle_key(key(KeyCode::Char(k)), now).is_empty());
        }
        assert_eq!((a.exposure, a.brightness), (Some(200), Some(40)));
    }

    #[test]
    fn keys_on_an_unknown_value_say_so() {
        let now = Instant::now();
        let exposure = [
            key(KeyCode::Right),
            shift(KeyCode::Right),
            key(KeyCode::PageUp),
            key(KeyCode::Char('[')),
        ];
        for k in exposure {
            let mut a = app();
            a.exposure = None;
            assert!(a.handle_key(k, now).is_empty(), "{k:?}");
            assert_eq!(a.message.as_deref(), Some(EXPOSURE_UNKNOWN), "{k:?}");
        }
        for k in [
            key(KeyCode::Right),
            key(KeyCode::PageDown),
            shift(KeyCode::Left),
        ] {
            let mut a = app();
            a.focus = Focus::Brightness;
            a.brightness = None;
            assert!(a.handle_key(k, now).is_empty(), "{k:?}");
            assert_eq!(
                a.message.as_deref(),
                Some("brightness value unknown — press r to reload"),
                "{k:?}"
            );
        }
        let mut a = app();
        a.focus = Focus::Brightness;
        a.brightness_range = None;
        assert!(a.handle_key(key(KeyCode::Right), now).is_empty());
        assert_eq!(a.message.as_deref(), Some("brightness range unknown"));

        for (focus, k) in [
            (Focus::Exposure, key(KeyCode::Char('a'))),
            (Focus::Mode, key(KeyCode::Right)),
        ] {
            let mut a = app();
            a.focus = focus;
            a.mode = None;
            assert!(a.handle_key(k, now).is_empty(), "{k:?}");
            assert_eq!(
                a.message.as_deref(),
                Some("mode unknown — press r to reload")
            );
        }
    }

    #[test]
    fn snap_refuses_a_range_without_a_flicker_free_value() {
        let now = Instant::now();
        for (range, value, k) in [((1, 50), 20, ']'), ((150, 250), 200, '[')] {
            let mut a = app();
            a.exposure_range = Ok(range);
            a.exposure = Some(value);
            assert!(a.handle_key(key(KeyCode::Char(k)), now).is_empty());
            assert_eq!(
                a.message,
                Some(format!("no flicker-free value in {}..{}", range.0, range.1))
            );
            assert_eq!(a.exposure, Some(value));
            assert!(a.tick(now + Duration::from_secs(1)).is_empty());
        }
        // An unknown value is reported first.
        let mut a = app();
        a.exposure_range = Ok((1, 50));
        a.exposure = None;
        a.handle_key(key(KeyCode::Char(']')), now);
        assert_eq!(a.message.as_deref(), Some(EXPOSURE_UNKNOWN));
    }

    #[test]
    fn tab_and_backtab_cycle_focus() {
        let mut a = app();
        let now = Instant::now();
        a.handle_key(key(KeyCode::Tab), now);
        assert_eq!(a.focus, Focus::Brightness);
        a.handle_key(key(KeyCode::Tab), now);
        assert_eq!(a.focus, Focus::Mode);
        a.handle_key(key(KeyCode::Tab), now);
        assert_eq!(a.focus, Focus::Exposure);
        a.handle_key(key(KeyCode::BackTab), now);
        assert_eq!(a.focus, Focus::Mode);
    }

    #[test]
    fn step_sizes() {
        let t0 = Instant::now();
        // Exposure: the arrows jump between flicker-free values, Shift steps by 10.
        let cases = [
            (200, key(KeyCode::Right), 300),
            (200, key(KeyCode::Left), 100),
            (227, key(KeyCode::Right), 300),
            (227, key(KeyCode::Left), 200),
            (227, shift(KeyCode::Right), 237),
            (200, shift(KeyCode::Right), 210),
            (200, shift(KeyCode::Left), 190),
            (200, key(KeyCode::PageUp), 300),
            (200, key(KeyCode::PageDown), 100),
        ];
        for (from, k, want) in cases {
            let mut a = app();
            a.exposure = Some(from);
            assert_eq!(
                exposure_writes(&a.handle_key(k, t0)),
                [want],
                "{from} {k:?}"
            );
        }
        // Brightness: the arrows step by 10, Shift by 1.
        let cases = [
            (key(KeyCode::Right), 50),
            (key(KeyCode::Left), 30),
            (shift(KeyCode::Right), 41),
            (shift(KeyCode::Left), 39),
        ];
        for (k, want) in cases {
            let mut a = app();
            a.focus = Focus::Brightness;
            let c = a.handle_key(k, t0);
            assert!(
                matches!(c[..], [Command::Brightness { value, .. }] if value == want),
                "{k:?}: {c:?}"
            );
        }
    }

    #[test]
    fn brightness_arrows_clamp_to_the_range() {
        let t0 = Instant::now();
        for (from, k, want) in [(250, key(KeyCode::Right), 255), (5, key(KeyCode::Left), 0)] {
            let mut a = app();
            a.focus = Focus::Brightness;
            a.brightness = Some(from);
            let c = a.handle_key(k, t0);
            assert!(
                matches!(c[..], [Command::Brightness { value, .. }] if value == want),
                "{from} {k:?}: {c:?}"
            );
        }
    }

    #[test]
    fn a_window_problem_survives_the_camera_and_goes_on_a_key() {
        let mut a = App {
            window_problem: Some("could not fit the window: xdotool not found".into()),
            message: Some("old".into()),
            ..App::default()
        };
        a.on_event(Event::Up {
            generation: 1,
            video_node: PathBuf::from("/dev/video0"),
            ranges: Ranges {
                exposure: Ok((1, 2500)),
                brightness: Ok((0, 255)),
            },
        });
        assert_eq!(a.message, None, "the camera clears the message");
        assert!(a.window_problem.is_some(), "but not the window problem");
        a.handle_key(key(KeyCode::Down), Instant::now());
        assert_eq!(a.window_problem, None);
    }

    #[test]
    fn up_and_down_cycle_focus() {
        let mut a = app();
        let now = Instant::now();
        a.handle_key(key(KeyCode::Down), now);
        assert_eq!(a.focus, Focus::Brightness);
        a.handle_key(key(KeyCode::Down), now);
        assert_eq!(a.focus, Focus::Mode);
        a.handle_key(key(KeyCode::Down), now);
        assert_eq!(a.focus, Focus::Exposure);
        a.handle_key(key(KeyCode::Up), now);
        assert_eq!(a.focus, Focus::Mode);
    }

    #[test]
    fn exposure_arrows_share_the_snap_refusals() {
        let now = Instant::now();
        let mut a = app();
        a.exposure = None;
        assert!(a.handle_key(key(KeyCode::Left), now).is_empty());
        assert_eq!(a.message.as_deref(), Some(EXPOSURE_UNKNOWN));
        let mut a = app();
        a.exposure_range = Ok((1, 50));
        a.exposure = Some(20);
        assert!(a.handle_key(key(KeyCode::Right), now).is_empty());
        assert_eq!(a.message.as_deref(), Some("no flicker-free value in 1..50"));
    }

    #[test]
    fn steps_clamp_to_the_device_range() {
        let mut a = app();
        let t = Instant::now();
        a.exposure = Some(2);
        assert_eq!(exposure_writes(&a.handle_key(shift(KeyCode::Left), t)), [1]);
    }

    #[test]
    fn exposure_in_auto_asks_for_shutter_first() {
        let mut a = app();
        a.mode = Some(Mode::Auto);
        let c = a.handle_key(key(KeyCode::Right), Instant::now());
        assert!(matches!(
            c[..],
            [Command::Exposure {
                value: 300,
                ensure_shutter: true,
                ..
            }]
        ));
        assert_eq!(a.mode, Some(Mode::Shutter));
    }

    #[test]
    fn throttle_sends_one_now_and_the_last_value_later() {
        let mut a = app();
        let t0 = Instant::now();
        assert_eq!(
            exposure_writes(&a.handle_key(shift(KeyCode::Right), t0)),
            [210]
        );
        let t1 = t0 + Duration::from_millis(10);
        assert!(a.handle_key(shift(KeyCode::Right), t1).is_empty());
        assert!(a.handle_key(shift(KeyCode::Right), t1).is_empty());
        assert!(a.tick(t0 + Duration::from_millis(20)).is_empty());
        assert_eq!(
            a.poll_timeout(t0 + Duration::from_millis(20)),
            Some(Duration::from_millis(10))
        );
        assert_eq!(exposure_writes(&a.tick(t0 + THROTTLE)), [230]);
        assert!(a.tick(t0 + THROTTLE * 3).is_empty(), "nothing left to send");
    }

    #[test]
    fn past_deadline_writes_now_and_timeout_saturates() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5));
        let late = t0 + Duration::from_millis(500);
        assert_eq!(a.poll_timeout(late), Some(Duration::ZERO));
        assert_eq!(exposure_writes(&a.tick(late)), [220]);
    }

    #[test]
    fn quit_flushes_a_held_write() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5));
        let c = a.handle_key(key(KeyCode::Char('q')), t0 + Duration::from_millis(6));
        assert_eq!(exposure_writes(&c), [220]);
        assert!(matches!(c.last(), Some(Command::Quit { .. })));
        assert!(a.quit);
    }

    #[test]
    fn switching_to_auto_cancels_a_held_write() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5));
        let c = a.handle_key(key(KeyCode::Char('a')), t0 + Duration::from_millis(6));
        assert!(matches!(
            c[..],
            [Command::Mode {
                mode: Mode::Auto,
                ..
            }]
        ));
        assert!(a.tick(t0 + THROTTLE * 2).is_empty());
    }

    #[test]
    fn r_cancels_held_writes_and_reads() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5));
        let c = a.handle_key(key(KeyCode::Char('r')), t0 + Duration::from_millis(6));
        assert!(matches!(c[..], [Command::ReadAll { .. }]));
        assert!(a.tick(t0 + THROTTLE * 2).is_empty());
    }

    #[test]
    fn number_entry() {
        let mut a = app();
        let t = Instant::now();
        a.handle_key(key(KeyCode::Enter), t);
        for k in [
            KeyCode::Char('3'),
            KeyCode::Char('9'),
            KeyCode::Backspace,
            KeyCode::Char('0'),
        ] {
            a.handle_key(key(k), t);
        }
        for ignored in ['q', 'a', 'r', 'x'] {
            assert!(a.handle_key(key(KeyCode::Char(ignored)), t).is_empty());
        }
        assert!(!a.quit, "q is ignored in entry");
        assert_eq!(a.entry.as_deref(), Some("30"));
        assert_eq!(exposure_writes(&a.handle_key(key(KeyCode::Enter), t)), [30]);
        assert_eq!(a.entry, None);
    }

    #[test]
    fn esc_cancels_entry_without_quitting_and_quits_outside_it() {
        let mut a = app();
        let t = Instant::now();
        a.handle_key(key(KeyCode::Char(':')), t);
        assert!(a.handle_key(key(KeyCode::Esc), t).is_empty());
        assert!(!a.quit);
        assert_eq!(a.entry, None);
        a.handle_key(key(KeyCode::Esc), t);
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits_in_and_out_of_entry() {
        let mut a = app();
        a.handle_key(key(KeyCode::Enter), Instant::now());
        a.handle_key(ctrl_c(), Instant::now());
        assert!(a.quit);
        let mut b = app();
        b.handle_key(ctrl_c(), Instant::now());
        assert!(b.quit);
    }

    #[test]
    fn out_of_range_and_oversized_input_is_rejected() {
        let t = Instant::now();
        for typed in ["0", "2501", "99999999999"] {
            let mut a = app();
            a.handle_key(key(KeyCode::Enter), t);
            for ch in typed.chars() {
                a.handle_key(key(KeyCode::Char(ch)), t);
            }
            assert!(a.handle_key(key(KeyCode::Enter), t).is_empty(), "{typed}");
            assert!(a.message.as_deref().unwrap().contains("1..2500"), "{typed}");
        }
    }

    #[test]
    fn mode_focus_keys() {
        let t = Instant::now();
        let mut a = app();
        a.focus = Focus::Mode;
        assert!(a.handle_key(key(KeyCode::Enter), t).is_empty());
        assert_eq!(a.entry, None, "no number entry on Mode");
        for k in [key(KeyCode::PageUp), key(KeyCode::PageDown)] {
            assert!(a.handle_key(k, t).is_empty(), "{k:?}");
        }
        assert!(a.handle_key(key(KeyCode::Char('[')), t).is_empty());
        assert!(a.handle_key(key(KeyCode::Char(']')), t).is_empty());
        let c = a.handle_key(key(KeyCode::Right), t);
        assert!(matches!(
            c[..],
            [Command::Mode {
                mode: Mode::Auto,
                ..
            }]
        ));
        let c = a.handle_key(shift(KeyCode::Left), t);
        assert!(matches!(
            c[..],
            [Command::Mode {
                mode: Mode::Shutter,
                ..
            }]
        ));
        let c = a.handle_key(key(KeyCode::Left), t);
        assert!(matches!(
            c[..],
            [Command::Mode {
                mode: Mode::Auto,
                ..
            }]
        ));
    }

    #[test]
    fn read_back_does_not_overwrite_a_newer_write() {
        let mut a = app();
        let t = Instant::now();
        let c = a.handle_key(key(KeyCode::PageUp), t);
        let Command::Exposure { rev, .. } = c[0] else {
            panic!()
        };
        let stale = Values {
            exposure: Ok(200),
            mode: Ok(Mode::Shutter),
            brightness: Ok(40),
        };
        a.on_event(Event::Values {
            generation: 1,
            rev: rev - 1,
            values: stale.clone(),
        });
        assert_eq!(a.exposure, Some(300), "older read ignored");
        let fresh = Values {
            exposure: Ok(299),
            ..stale
        };
        a.on_event(Event::Values {
            generation: 1,
            rev,
            values: fresh,
        });
        assert_eq!(
            a.exposure,
            Some(299),
            "the camera's value wins once it answers our write"
        );
    }

    #[test]
    fn results_from_an_old_generation_are_dropped() {
        let mut a = app();
        let v = Values {
            exposure: Ok(5),
            mode: Ok(Mode::Auto),
            brightness: Ok(1),
        };
        a.on_event(Event::Values {
            generation: 0,
            rev: 99,
            values: v,
        });
        assert_eq!(a.exposure, Some(200));
    }

    #[test]
    fn unknown_range_refuses_exposure() {
        let mut a = app();
        a.exposure_range = Err("GET_MAX failed".into());
        assert!(a.handle_key(key(KeyCode::Right), Instant::now()).is_empty());
        assert!(a.message.as_deref().unwrap().contains("GET_MAX failed"));
    }

    #[test]
    fn gone_clears_values_and_held_writes() {
        let mut a = app();
        let t = Instant::now();
        a.handle_key(key(KeyCode::Right), t);
        a.handle_key(key(KeyCode::Right), t + Duration::from_millis(1));
        a.on_event(Event::Gone {
            generation: 1,
            reason: "camera disconnected".into(),
        });
        assert_eq!(a.exposure, None);
        assert!(a.tick(t + THROTTLE * 2).is_empty());
        assert_eq!(a.preview, Preview::NoCamera);
    }

    #[test]
    fn a_newer_write_sends_the_held_exposure_first() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5)); // held
        a.handle_key(key(KeyCode::Tab), t0 + Duration::from_millis(6));
        let c = a.handle_key(key(KeyCode::Right), t0 + Duration::from_millis(7));
        let revs: Vec<u64> = c
            .iter()
            .map(|c| match c {
                Command::Exposure { rev, .. } | Command::Brightness { rev, .. } => *rev,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert!(matches!(
            c[..],
            [
                Command::Exposure { value: 220, .. },
                Command::Brightness { .. }
            ]
        ));
        assert!(revs[0] < revs[1], "revisions leave in order: {revs:?}");
        assert!(a.tick(t0 + THROTTLE * 2).is_empty(), "nothing still held");
    }

    #[test]
    fn switching_to_shutter_sends_the_held_exposure_first() {
        let mut a = app();
        let t0 = Instant::now();
        a.handle_key(shift(KeyCode::Right), t0);
        a.handle_key(shift(KeyCode::Right), t0 + Duration::from_millis(5)); // held
        a.mode = Some(Mode::Auto); // a read-back flipped the shown mode
        let c = a.handle_key(key(KeyCode::Char('a')), t0 + Duration::from_millis(6));
        assert!(matches!(
            c[..],
            [
                Command::Exposure { value: 220, .. },
                Command::Mode {
                    mode: Mode::Shutter,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn values_after_a_reconnect_are_accepted_even_if_a_write_was_lost() {
        let mut a = app();
        let t = Instant::now();
        // A write the worker never applies: the device goes away first.
        a.handle_key(key(KeyCode::PageUp), t);
        a.on_event(Event::Gone {
            generation: 1,
            reason: "camera disconnected".into(),
        });
        a.on_event(Event::Up {
            generation: 2,
            video_node: PathBuf::from("/dev/video0"),
            ranges: Ranges {
                exposure: Ok((1, 2500)),
                brightness: Ok((0, 255)),
            },
        });
        // The worker's revision never reached the lost write.
        a.on_event(Event::Values {
            generation: 2,
            rev: 0,
            values: Values {
                exposure: Ok(200),
                mode: Ok(Mode::Shutter),
                brightness: Ok(40),
            },
        });
        assert_eq!(a.exposure, Some(200));
        assert_eq!(a.brightness, Some(40));
        assert_eq!(a.mode, Some(Mode::Shutter));
    }

    #[test]
    fn range_and_read_errors_are_shown() {
        let mut a = App::default();
        a.on_event(Event::Up {
            generation: 1,
            video_node: PathBuf::from("/dev/video0"),
            ranges: Ranges {
                exposure: Err("no write access to /dev/bus/usb/006/007".into()),
                brightness: Ok((0, 255)),
            },
        });
        assert!(a.message.as_deref().unwrap().contains("no write access"));

        let mut b = app();
        b.on_event(Event::Values {
            generation: 1,
            rev: 0,
            values: Values {
                exposure: Ok(200),
                mode: Err("mode read failed".into()),
                brightness: Ok(40),
            },
        });
        assert!(b.message.as_deref().unwrap().contains("mode read failed"));

        let mut c = app();
        c.on_event(Event::Values {
            generation: 1,
            rev: 0,
            values: Values {
                exposure: Ok(200),
                mode: Ok(Mode::Shutter),
                brightness: Err("brightness read failed".into()),
            },
        });
        assert!(
            c.message
                .as_deref()
                .unwrap()
                .contains("brightness read failed")
        );
    }

    #[test]
    fn failed_shutter_switch_drops_the_held_value() {
        let mut a = app();
        let t = Instant::now();
        a.handle_key(key(KeyCode::Right), t);
        a.handle_key(key(KeyCode::Right), t + Duration::from_millis(1));
        a.on_event(Event::Failed {
            generation: 1,
            rev: 99,
            target: Target::Exposure,
            error: "could not switch to Shutter Priority: EIO".into(),
        });
        assert!(a.tick(t + THROTTLE * 2).is_empty());
        assert!(a.message.as_deref().unwrap().contains("Shutter"));
    }
}
