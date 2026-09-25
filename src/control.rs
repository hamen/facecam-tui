//! The control worker: owns discovery, the usbfs fd and the V4L2 control fd.
//!
//! The UI never queues commands. It writes the newest desired value of each control, tagged with
//! one global revision number (`rev`), into [`Desired`]. The worker takes the whole struct at
//! once and applies it in `rev` order, so an older value is never written after a newer one and
//! there is no backlog to drain.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

use nix::errno::Errno;

use crate::{
    camera::{Camera, Facecam, Mode},
    usb,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Exposure {
        rev: u64,
        value: u32,
        ensure_shutter: bool,
    },
    Brightness {
        rev: u64,
        value: i64,
    },
    /// Switching to Auto also cancels an older pending exposure write.
    Mode {
        rev: u64,
        mode: Mode,
    },
    /// A barrier: everything older is applied, then all values are read.
    ReadAll {
        rev: u64,
    },
    /// A barrier: everything older is applied, then the worker stops.
    Quit {
        rev: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposureWrite {
    pub rev: u64,
    pub value: u32,
    pub ensure_shutter: bool,
}

/// The newest wanted value per control. One struct under one mutex.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Desired {
    pub exposure: Option<ExposureWrite>,
    pub brightness: Option<(u64, i64)>,
    pub mode: Option<(u64, Mode)>,
    pub read_all: Option<u64>,
    pub quit: Option<u64>,
    /// Set by the capture thread when its stream fails: check that the device still exists.
    pub check: bool,
}

impl Desired {
    pub fn submit(&mut self, command: Command) {
        match command {
            Command::Exposure {
                rev,
                value,
                ensure_shutter,
            } => {
                self.exposure = Some(ExposureWrite {
                    rev,
                    value,
                    ensure_shutter,
                });
            }
            Command::Brightness { rev, value } => self.brightness = Some((rev, value)),
            Command::Mode { rev, mode } => {
                if mode == Mode::Auto {
                    self.exposure = None;
                }
                self.mode = Some((rev, mode));
            }
            // A barrier, not a cancel: older writes are applied first, then everything is read.
            // (The UI drops its own held exposure before it sends this.)
            Command::ReadAll { rev } => self.read_all = Some(rev),
            Command::Quit { rev } => self.quit = Some(rev),
        }
    }

    pub fn has_work(&self) -> bool {
        self.exposure.is_some()
            || self.brightness.is_some()
            || self.mode.is_some()
            || self.read_all.is_some()
            || self.quit.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Exposure,
    Brightness,
    Mode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Values {
    pub exposure: Result<u32, String>,
    pub mode: Result<Mode, String>,
    pub brightness: Result<i64, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranges {
    pub exposure: Result<(u32, u32), String>,
    pub brightness: Result<(i64, i64), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Up {
        generation: u64,
        video_node: PathBuf,
        ranges: Ranges,
    },
    Gone {
        generation: u64,
        reason: String,
    },
    /// Values read from the camera. `rev` is the newest intent the worker had applied before
    /// the read; the UI ignores a value older than its own newest write to that control.
    Values {
        generation: u64,
        rev: u64,
        values: Values,
    },
    Failed {
        generation: u64,
        rev: u64,
        target: Target,
        error: String,
    },
    QuitDone,
}

/// Errors that mean the camera is no longer there.
pub fn is_gone(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(Errno::from_raw),
        Some(Errno::ENODEV | Errno::ESHUTDOWN | Errno::ENXIO)
    )
}

/// What one batch did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub max_rev: u64,
    pub failures: Vec<(u64, Target, String)>,
    pub read_requested: bool,
}

enum Op {
    Exposure(ExposureWrite),
    Brightness(i64),
    Mode(Mode),
    ReadAll,
}

/// Applies one batch in `rev` order. Returns `Err` only when the camera is gone.
pub fn apply(camera: &mut dyn Camera, batch: &Desired) -> Result<Applied, io::Error> {
    let mut exposure = batch.exposure;
    // A newer Auto cancels the older exposure write. A newer Shutter keeps ensure_shutter:
    // the exposure runs first (older rev), so it still needs Shutter Priority in place.
    if let (Some(e), Some((mode_rev, Mode::Auto))) = (exposure.as_ref(), batch.mode)
        && mode_rev > e.rev
    {
        exposure = None;
    }

    let mut ops: Vec<(u64, Op)> = Vec::new();
    if let Some(e) = exposure {
        ops.push((e.rev, Op::Exposure(e)));
    }
    if let Some((rev, value)) = batch.brightness {
        ops.push((rev, Op::Brightness(value)));
    }
    if let Some((rev, mode)) = batch.mode {
        ops.push((rev, Op::Mode(mode)));
    }
    if let Some(rev) = batch.read_all {
        ops.push((rev, Op::ReadAll));
    }
    ops.sort_by_key(|(rev, _)| *rev);

    let mut applied = Applied {
        max_rev: batch.quit.unwrap_or(0),
        ..Applied::default()
    };
    for (rev, op) in ops {
        applied.max_rev = applied.max_rev.max(rev);
        let (target, result) = match op {
            Op::Exposure(e) => {
                if e.ensure_shutter
                    && let Err(err) = camera.set_mode(Mode::Shutter)
                {
                    if is_gone(&err) {
                        return Err(err);
                    }
                    let msg = format!("could not switch to Shutter Priority: {err}");
                    applied.failures.push((rev, Target::Exposure, msg));
                    continue;
                }
                (Target::Exposure, camera.set_exposure(e.value))
            }
            Op::Brightness(value) => (Target::Brightness, camera.set_brightness(value)),
            Op::Mode(mode) => (Target::Mode, camera.set_mode(mode)),
            Op::ReadAll => {
                applied.read_requested = true;
                continue;
            }
        };
        if let Err(err) = result {
            if is_gone(&err) {
                return Err(err);
            }
            applied.failures.push((rev, target, err.to_string()));
        }
    }
    Ok(applied)
}

/// Reads the three values. `Err` only when the camera is gone.
pub fn read_values(camera: &mut dyn Camera) -> Result<Values, io::Error> {
    fn keep<T>(r: io::Result<T>) -> Result<Result<T, String>, io::Error> {
        match r {
            Err(e) if is_gone(&e) => Err(e),
            other => Ok(other.map_err(|e| e.to_string())),
        }
    }
    Ok(Values {
        exposure: keep(camera.exposure())?,
        mode: keep(camera.mode())?,
        brightness: keep(camera.brightness())?,
    })
}

/// The UI side: the shared desired state and the worker's wake-up.
#[derive(Clone, Default)]
pub struct Shared {
    inner: Arc<(Mutex<Desired>, Condvar)>,
}

impl Shared {
    pub fn submit(&self, commands: impl IntoIterator<Item = Command>) {
        let (lock, wake) = &*self.inner;
        let mut desired = lock.lock().unwrap_or_else(|e| e.into_inner());
        for command in commands {
            desired.submit(command);
        }
        wake.notify_one();
    }

    /// Called by the capture thread when its stream fails.
    pub fn request_check(&self) {
        let (lock, wake) = &*self.inner;
        lock.lock().unwrap_or_else(|e| e.into_inner()).check = true;
        wake.notify_one();
    }

    /// Waits up to `timeout` for work, then takes everything.
    fn take(&self, timeout: Duration) -> Desired {
        let (lock, wake) = &*self.inner;
        let mut desired = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !desired.has_work() && !desired.check {
            desired = wake
                .wait_timeout(desired, timeout)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        std::mem::take(&mut *desired)
    }

    fn is_idle(&self) -> bool {
        !self
            .inner
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_work()
    }

    /// Drops pending writes after the device went away; a quit request survives.
    fn clear_writes(&self) {
        let mut desired = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        let quit = desired.quit;
        *desired = Desired {
            quit,
            ..Desired::default()
        };
    }

    fn quit_requested(&self) -> bool {
        self.inner
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .quit
            .is_some()
    }
}

const POLL: Duration = Duration::from_millis(200);
const RETRY: Duration = Duration::from_secs(2);
/// The udev rule re-applies exposure about 2 s after plug-in; read again after it.
const REREAD_AFTER_UP: Duration = Duration::from_secs(3);
const PRESENCE_CHECK: Duration = Duration::from_secs(1);

/// The worker thread body. Returns after it sent [`Event::QuitDone`].
pub fn run(shared: Shared, events: Sender<Event>, sysfs: PathBuf, dev: PathBuf) {
    let mut generation = 0;
    let mut last_rev = 0;
    let mut gone_reason: Option<String> = None;

    loop {
        if shared.quit_requested() {
            let _ = events.send(Event::QuitDone);
            return;
        }
        let opened = match usb::discover(&sysfs, &dev) {
            None => Err("Facecam not found".to_string()),
            Some(found) => match &found.video_node {
                None => Err("Facecam has no video node yet".to_string()),
                Some(video) => Facecam::open(&found.usb_node, video)
                    .map(|cam| (found.clone(), video.clone(), cam))
                    .map_err(|e| format!("{}: {e}", video.display())),
            },
        };
        let (found, video_node, mut camera) = match opened {
            Ok(opened) => opened,
            Err(reason) => {
                if gone_reason.as_deref() != Some(reason.as_str()) {
                    let _ = events.send(Event::Gone {
                        generation,
                        reason: reason.clone(),
                    });
                    gone_reason = Some(reason);
                }
                wait_unless_quit(&shared, RETRY);
                continue;
            }
        };

        generation += 1;
        let ranges = Ranges {
            exposure: camera.exposure_range().map_err(|e| e.to_string()),
            brightness: camera.brightness_range().map_err(|e| e.to_string()),
        };
        let _ = events.send(Event::Up {
            generation,
            video_node,
            ranges,
        });

        let present = || usb::check_presence(&sysfs, &dev, &found);
        let reason = serve(
            &shared,
            &events,
            &mut camera,
            &present,
            generation,
            &mut last_rev,
        );
        match reason {
            Served::Quit => {
                let _ = events.send(Event::QuitDone);
                return;
            }
            Served::Gone(reason) => {
                shared.clear_writes();
                let _ = events.send(Event::Gone {
                    generation,
                    reason: reason.clone(),
                });
                gone_reason = Some(reason);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Served {
    Quit,
    Gone(String),
}

fn serve(
    shared: &Shared,
    events: &Sender<Event>,
    camera: &mut dyn Camera,
    present: &dyn Fn() -> usb::Presence,
    generation: u64,
    last_rev: &mut u64,
) -> Served {
    let send_values = |camera: &mut dyn Camera, rev: u64| -> Result<(), io::Error> {
        let values = read_values(camera)?;
        let _ = events.send(Event::Values {
            generation,
            rev,
            values,
        });
        Ok(())
    };
    let gone = |e: io::Error| Served::Gone(format!("camera disconnected: {e}"));

    if let Err(e) = send_values(camera, *last_rev) {
        return gone(e);
    }
    let mut reread_at = Some(Instant::now() + REREAD_AFTER_UP);
    let mut presence_at = Instant::now() + PRESENCE_CHECK;
    let mut changed_once = false;

    loop {
        let batch = shared.take(POLL);
        let now = Instant::now();

        if batch.check || now >= presence_at {
            presence_at = now + PRESENCE_CHECK;
            match present() {
                usb::Presence::Same => changed_once = false,
                usb::Presence::Disconnected => {
                    return Served::Gone("camera disconnected".to_string());
                }
                // One differing read can be a failed sysfs read; two in a row is a rebind
                // with other nodes, and the session on the old nodes is dead.
                usb::Presence::Changed if changed_once => {
                    return Served::Gone("camera nodes changed".to_string());
                }
                usb::Presence::Changed => changed_once = true,
            }
        }

        if batch.has_work() {
            let applied = match apply(camera, &batch) {
                Ok(applied) => applied,
                Err(e) => return gone(e),
            };
            *last_rev = (*last_rev).max(applied.max_rev);
            for (rev, target, error) in &applied.failures {
                let _ = events.send(Event::Failed {
                    generation,
                    rev: *rev,
                    target: *target,
                    error: error.clone(),
                });
            }
            if batch.quit.is_some() {
                return Served::Quit;
            }
            // Read back when asked, after a failure, or once the UI has stopped writing.
            if (applied.read_requested || !applied.failures.is_empty() || shared.is_idle())
                && let Err(e) = send_values(camera, *last_rev)
            {
                return gone(e);
            }
        }

        if reread_at.is_some_and(|at| now >= at) {
            reread_at = None;
            if let Err(e) = send_values(camera, *last_rev) {
                return gone(e);
            }
        }
    }
}

fn wait_unless_quit(shared: &Shared, total: Duration) {
    let end = Instant::now() + total;
    while Instant::now() < end && !shared.quit_requested() {
        std::thread::sleep(POLL.min(end.saturating_duration_since(Instant::now())));
    }
}

/// Default roots for [`run`].
pub fn system_roots() -> (PathBuf, PathBuf) {
    (
        Path::new("/sys").to_path_buf(),
        Path::new("/dev").to_path_buf(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Exposure(u32),
        Mode(Mode),
        Brightness(i64),
    }

    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        fail_mode: Option<i32>,
        fail_brightness: bool,
    }

    fn errno(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    impl Camera for Fake {
        fn exposure(&mut self) -> io::Result<u32> {
            Ok(200)
        }
        fn exposure_range(&mut self) -> io::Result<(u32, u32)> {
            Ok((1, 2500))
        }
        fn set_exposure(&mut self, value: u32) -> io::Result<()> {
            self.calls.push(Call::Exposure(value));
            Ok(())
        }
        fn mode(&mut self) -> io::Result<Mode> {
            Ok(Mode::Shutter)
        }
        fn set_mode(&mut self, mode: Mode) -> io::Result<()> {
            if let Some(code) = self.fail_mode {
                return Err(errno(code));
            }
            self.calls.push(Call::Mode(mode));
            Ok(())
        }
        fn brightness(&mut self) -> io::Result<i64> {
            Ok(0)
        }
        fn brightness_range(&mut self) -> io::Result<(i64, i64)> {
            Ok((0, 255))
        }
        fn set_brightness(&mut self, value: i64) -> io::Result<()> {
            if self.fail_brightness {
                return Err(errno(22));
            }
            self.calls.push(Call::Brightness(value));
            Ok(())
        }
    }

    fn desired(commands: &[Command]) -> Desired {
        let mut d = Desired::default();
        for c in commands {
            d.submit(*c);
        }
        d
    }

    #[test]
    fn applies_in_rev_order_across_controls() {
        let mut cam = Fake::default();
        let batch = desired(&[
            Command::Brightness { rev: 3, value: 40 },
            Command::Exposure {
                rev: 1,
                value: 200,
                ensure_shutter: false,
            },
            Command::Mode {
                rev: 2,
                mode: Mode::Shutter,
            },
        ]);
        let applied = apply(&mut cam, &batch).unwrap();
        assert_eq!(
            cam.calls,
            [
                Call::Exposure(200),
                Call::Mode(Mode::Shutter),
                Call::Brightness(40)
            ]
        );
        assert_eq!(applied.max_rev, 3);
    }

    #[test]
    fn ensure_shutter_sets_the_mode_before_the_exposure() {
        let mut cam = Fake::default();
        let batch = desired(&[Command::Exposure {
            rev: 1,
            value: 300,
            ensure_shutter: true,
        }]);
        apply(&mut cam, &batch).unwrap();
        assert_eq!(cam.calls, [Call::Mode(Mode::Shutter), Call::Exposure(300)]);
    }

    #[test]
    fn failed_shutter_switch_skips_the_exposure_and_reports() {
        let mut cam = Fake {
            fail_mode: Some(13),
            ..Fake::default()
        };
        let batch = desired(&[Command::Exposure {
            rev: 5,
            value: 300,
            ensure_shutter: true,
        }]);
        let applied = apply(&mut cam, &batch).unwrap();
        assert!(
            cam.calls.is_empty(),
            "exposure must not be written: {:?}",
            cam.calls
        );
        assert_eq!(applied.failures.len(), 1);
        assert_eq!(applied.failures[0].0, 5);
        assert_eq!(applied.failures[0].1, Target::Exposure);
    }

    #[test]
    fn newer_auto_cancels_an_older_exposure() {
        // Worker-side guard: a stale exposure next to a newer Auto is never written.
        let mut batch = Desired {
            exposure: Some(ExposureWrite {
                rev: 1,
                value: 300,
                ensure_shutter: true,
            }),
            ..Desired::default()
        };
        batch.mode = Some((2, Mode::Auto));
        let mut cam = Fake::default();
        apply(&mut cam, &batch).unwrap();
        assert_eq!(cam.calls, [Call::Mode(Mode::Auto)]);
    }

    #[test]
    fn older_mode_does_not_undo_a_newer_exposure() {
        let mut cam = Fake::default();
        let batch = desired(&[
            Command::Mode {
                rev: 1,
                mode: Mode::Auto,
            },
            Command::Exposure {
                rev: 2,
                value: 250,
                ensure_shutter: true,
            },
        ]);
        apply(&mut cam, &batch).unwrap();
        assert_eq!(
            cam.calls,
            [
                Call::Mode(Mode::Auto),
                Call::Mode(Mode::Shutter),
                Call::Exposure(250)
            ]
        );
    }

    #[test]
    fn newer_shutter_mode_keeps_the_ensure_for_the_older_exposure() {
        let mut batch = desired(&[Command::Exposure {
            rev: 1,
            value: 250,
            ensure_shutter: true,
        }]);
        batch.mode = Some((2, Mode::Shutter));
        let mut cam = Fake::default();
        apply(&mut cam, &batch).unwrap();
        assert_eq!(
            cam.calls,
            [
                Call::Mode(Mode::Shutter),
                Call::Exposure(250),
                Call::Mode(Mode::Shutter)
            ]
        );
    }

    #[test]
    fn auto_command_clears_pending_exposure_in_desired() {
        let d = desired(&[
            Command::Exposure {
                rev: 1,
                value: 300,
                ensure_shutter: true,
            },
            Command::Mode {
                rev: 2,
                mode: Mode::Auto,
            },
        ]);
        assert_eq!(d.exposure, None);
        assert_eq!(d.mode, Some((2, Mode::Auto)));
    }

    #[test]
    fn read_all_is_a_barrier_that_applies_older_writes_first() {
        let batch = desired(&[
            Command::Exposure {
                rev: 1,
                value: 300,
                ensure_shutter: false,
            },
            Command::Brightness { rev: 2, value: 10 },
            Command::ReadAll { rev: 3 },
        ]);
        assert_eq!(batch.read_all, Some(3));
        let mut cam = Fake::default();
        let applied = apply(&mut cam, &batch).unwrap();
        assert_eq!(cam.calls, [Call::Exposure(300), Call::Brightness(10)]);
        assert!(applied.read_requested);
        assert_eq!(applied.max_rev, 3);
    }

    #[test]
    fn newest_value_wins_within_a_control() {
        let d = desired(&[
            Command::Exposure {
                rev: 1,
                value: 100,
                ensure_shutter: false,
            },
            Command::Exposure {
                rev: 2,
                value: 300,
                ensure_shutter: false,
            },
        ]);
        assert_eq!(d.exposure.map(|e| e.value), Some(300));
    }

    #[test]
    fn quit_is_a_barrier_that_still_flushes_writes() {
        let mut cam = Fake::default();
        let batch = desired(&[
            Command::Exposure {
                rev: 7,
                value: 180,
                ensure_shutter: false,
            },
            Command::Quit { rev: 8 },
        ]);
        let applied = apply(&mut cam, &batch).unwrap();
        assert_eq!(cam.calls, [Call::Exposure(180)]);
        assert_eq!(applied.max_rev, 8);
    }

    #[test]
    fn plain_failures_are_reported_and_gone_errors_abort() {
        let mut cam = Fake {
            fail_brightness: true,
            ..Fake::default()
        };
        let applied = apply(
            &mut cam,
            &desired(&[Command::Brightness { rev: 1, value: 9 }]),
        )
        .unwrap();
        assert_eq!(applied.failures[0].1, Target::Brightness);

        let mut cam = Fake {
            fail_mode: Some(19),
            ..Fake::default()
        };
        let batch = desired(&[Command::Mode {
            rev: 1,
            mode: Mode::Auto,
        }]);
        assert!(
            apply(&mut cam, &batch).is_err(),
            "ENODEV means the camera is gone"
        );
    }

    /// Runs `serve` with the fake camera in a thread, as the worker does.
    struct Harness {
        shared: Shared,
        events: std::sync::mpsc::Receiver<Event>,
        handle: std::thread::JoinHandle<Served>,
        /// Answers for the next presence checks, in order; `Same` once it is empty.
        presence: Arc<Mutex<std::collections::VecDeque<usb::Presence>>>,
        checks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Harness {
        /// Queues the presence answers, asks for a check, and waits until that check ran.
        fn check(&self, answers: &[usb::Presence]) {
            self.presence.lock().unwrap().extend(answers);
            let before = self.checks.load(std::sync::atomic::Ordering::SeqCst);
            self.shared.request_check();
            let end = Instant::now() + Duration::from_secs(2);
            while self.checks.load(std::sync::atomic::Ordering::SeqCst) == before {
                assert!(Instant::now() < end, "no presence check ran");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn serve_fake(cam: Fake) -> Harness {
        let shared = Shared::default();
        let (tx, events) = std::sync::mpsc::channel();
        let presence = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread_shared = shared.clone();
        let (thread_presence, thread_checks) = (presence.clone(), checks.clone());
        let handle = std::thread::spawn(move || {
            let mut cam = cam;
            let mut last_rev = 0;
            let present = || {
                let answer = thread_presence
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(usb::Presence::Same);
                thread_checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                answer
            };
            serve(&thread_shared, &tx, &mut cam, &present, 1, &mut last_rev)
        });
        Harness {
            shared,
            events,
            handle,
            presence,
            checks,
        }
    }

    fn next_event(events: &std::sync::mpsc::Receiver<Event>) -> Event {
        events
            .recv_timeout(Duration::from_secs(2))
            .expect("no event from serve")
    }

    fn values_rev(event: Event) -> u64 {
        match event {
            Event::Values { rev, .. } => rev,
            other => panic!("expected Values, got {other:?}"),
        }
    }

    #[test]
    fn serve_reads_on_start_and_after_an_idle_write() {
        let s = serve_fake(Fake::default());
        assert_eq!(values_rev(next_event(&s.events)), 0, "initial read");
        s.shared.submit([Command::Brightness { rev: 4, value: 10 }]);
        assert_eq!(values_rev(next_event(&s.events)), 4, "read-back once idle");
        s.shared.submit([Command::Quit { rev: 5 }]);
        assert_eq!(s.handle.join().unwrap(), Served::Quit);
    }

    #[test]
    fn serve_reports_a_failure_then_reads_back() {
        let s = serve_fake(Fake {
            fail_brightness: true,
            ..Fake::default()
        });
        next_event(&s.events);
        s.shared.submit([Command::Brightness { rev: 2, value: 10 }]);
        assert!(matches!(
            next_event(&s.events),
            Event::Failed {
                rev: 2,
                target: Target::Brightness,
                ..
            }
        ));
        assert_eq!(values_rev(next_event(&s.events)), 2);
        s.shared.submit([Command::Quit { rev: 3 }]);
        assert_eq!(s.handle.join().unwrap(), Served::Quit);
    }

    #[test]
    fn serve_reads_on_request_and_quits_without_a_read() {
        let s = serve_fake(Fake::default());
        next_event(&s.events);
        s.shared.submit([Command::ReadAll { rev: 7 }]);
        assert_eq!(values_rev(next_event(&s.events)), 7);
        s.shared.submit([
            Command::Exposure {
                rev: 8,
                value: 150,
                ensure_shutter: false,
            },
            Command::Quit { rev: 9 },
        ]);
        assert_eq!(s.handle.join().unwrap(), Served::Quit);
        assert!(s.events.try_recv().is_err(), "quit sends no read-back");
    }

    #[test]
    fn serve_notices_the_usb_node_disappearing() {
        let s = serve_fake(Fake::default());
        next_event(&s.events);
        s.presence
            .lock()
            .unwrap()
            .push_back(usb::Presence::Disconnected);
        s.shared.request_check();
        assert_eq!(
            s.handle.join().unwrap(),
            Served::Gone("camera disconnected".into())
        );
    }

    #[test]
    fn one_changed_read_does_not_end_the_session() {
        let s = serve_fake(Fake::default());
        next_event(&s.events);
        s.check(&[usb::Presence::Changed]);
        s.check(&[usb::Presence::Same]);
        s.check(&[usb::Presence::Changed]);
        s.shared.submit([Command::ReadAll { rev: 7 }]);
        assert_eq!(values_rev(next_event(&s.events)), 7);
        s.shared.submit([Command::Quit { rev: 8 }]);
        assert_eq!(s.handle.join().unwrap(), Served::Quit);
    }

    #[test]
    fn two_changed_reads_in_a_row_end_the_session() {
        let s = serve_fake(Fake::default());
        next_event(&s.events);
        s.check(&[usb::Presence::Changed]);
        s.presence.lock().unwrap().push_back(usb::Presence::Changed);
        s.shared.request_check();
        assert_eq!(
            s.handle.join().unwrap(),
            Served::Gone("camera nodes changed".into())
        );
    }

    #[test]
    fn gone_errnos() {
        assert!(is_gone(&errno(19)));
        assert!(is_gone(&errno(108)));
        assert!(!is_gone(&errno(13)));
        assert!(
            !is_gone(&errno(32)),
            "EPIPE is a stall (rejected request), not an unplug"
        );
    }
}
