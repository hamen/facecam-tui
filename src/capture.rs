//! The capture thread: its own V4L2 fd, MJPEG 960x540 @ 30 fps, newest frame only.

use std::{
    io,
    mem::ManuallyDrop,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use image::DynamicImage;
use v4l::{
    Device, Format, FourCC, Fraction,
    buffer::Type,
    io::{mmap::Stream, traits::CaptureStream},
    video::Capture as _,
};

use crate::control::{Shared, is_gone};

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 540;
const FPS: u32 = 30;
/// ~15 fps preview: frames arriving faster are dropped before they are decoded.
const DECODE_EVERY: Duration = Duration::from_millis(66);
const POLL_MS: i32 = 200;
const RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Starting,
    Streaming,
    /// EBUSY: another app holds the stream.
    Busy,
    NoAccess(String),
    Gone(String),
    Error(String),
}

pub struct Frame {
    pub seq: u64,
    pub image: DynamicImage,
}

/// The newest decoded frame; the lock is held only to swap it.
type Slot = Arc<Mutex<Option<Frame>>>;

/// One capture thread. Its frame slot and status channel belong to it alone: a thread that
/// outlives [`Capture::stop`] (stuck in the kernel) writes into a slot and a channel nobody reads
/// any more, never into those of the capture that replaced it.
pub struct Capture {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    slot: Slot,
    statuses: Receiver<Status>,
}

impl Capture {
    pub fn start(video: PathBuf, shared: Shared) -> Self {
        Self::start_with(
            |body| {
                std::thread::Builder::new()
                    .name("capture".into())
                    .spawn(body)
            },
            video,
            shared,
        )
    }

    fn start_with(
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
        video: PathBuf,
        shared: Shared,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let slot = Slot::default();
        let (status, statuses) = mpsc::channel();
        // A failed spawn drops the closure and the sender inside it, so report on a clone.
        let spawn_status = status.clone();
        let (thread_stop, thread_slot) = (stop.clone(), slot.clone());
        let body = Box::new(move || {
            run(
                &video,
                &thread_slot,
                &status,
                &shared,
                &thread_stop,
                stream_once,
                RETRY,
            );
        });
        let handle = match spawn(body) {
            Ok(handle) => Some(handle),
            Err(e) => {
                let _ = spawn_status.send(Status::Error(format!(
                    "could not start the capture thread: {e}"
                )));
                None
            }
        };
        Self {
            stop,
            handle,
            slot,
            statuses,
        }
    }

    /// Takes the newest decoded frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// The next status report, if any.
    pub fn try_status(&self) -> Option<Status> {
        self.statuses.try_recv().ok()
    }

    /// Asks the thread to stop and waits at most `timeout` for it; a thread stuck in the
    /// kernel is left behind rather than blocking the UI.
    pub fn stop(mut self, timeout: Duration) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let end = Instant::now() + timeout;
            while !handle.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

/// Opens and streams one device.
type StreamFn = fn(&PathBuf, &Slot, &Sender<Status>, &AtomicBool, &mut u64) -> io::Result<()>;

fn run(
    video: &PathBuf,
    slot: &Slot,
    status: &Sender<Status>,
    shared: &Shared,
    stop: &AtomicBool,
    stream: StreamFn,
    retry_after: Duration,
) {
    let mut seq = 0;
    // Once per capture: sending it before each retry made the pane alternate between
    // "starting" and "camera in use" every retry.
    let _ = status.send(Status::Starting);
    while !stop.load(Ordering::Relaxed) {
        let outcome = stream(video, slot, status, stop, &mut seq);
        let report = match outcome {
            Ok(()) => return, // stopped on request
            Err(e) => classify(&e),
        };
        if matches!(report, Status::Gone(_)) {
            shared.request_check();
        }
        let retry = retries(&report);
        let _ = status.send(report);
        if !retry {
            return; // reported once; a new Capture starts on the next DeviceUp
        }
        let end = Instant::now() + retry_after;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(POLL_MS as u64));
        }
    }
}

/// Busy (another app holds the stream) and Gone (the worker decides about the device) are
/// retried every 2 s. Any other error is reported once and the thread ends: retrying a format
/// the driver refuses would only repeat the same failure.
fn retries(status: &Status) -> bool {
    matches!(status, Status::Busy | Status::Gone(_))
}

fn classify(e: &io::Error) -> Status {
    match e.raw_os_error() {
        Some(16) => Status::Busy,
        Some(13) => Status::NoAccess(e.to_string()),
        _ if is_gone(e) || e.kind() == io::ErrorKind::NotFound => Status::Gone(e.to_string()),
        _ => Status::Error(e.to_string()),
    }
}

/// Checks what the driver actually set; anything else is an error, not a guess.
pub fn check_format(format: &Format, interval: Fraction) -> Result<(), String> {
    if format.fourcc != FourCC::new(b"MJPG") || format.width != WIDTH || format.height != HEIGHT {
        return Err(format!(
            "driver set {} {}x{}, wanted MJPG {WIDTH}x{HEIGHT}",
            format.fourcc, format.width, format.height
        ));
    }
    // A zero numerator would pass as 0 * 30 == 0, and a huge one would overflow the product.
    if interval.numerator == 0 || interval.numerator.checked_mul(FPS) != Some(interval.denominator)
    {
        return Err(format!(
            "driver set frame interval {interval}, wanted 1/{FPS}"
        ));
    }
    Ok(())
}

fn stream_once(
    video: &PathBuf,
    slot: &Slot,
    status: &Sender<Status>,
    stop: &AtomicBool,
    seq: &mut u64,
) -> io::Result<()> {
    // v4l opens with O_NONBLOCK.
    let dev = Device::with_path(video)?;
    let mut format = dev.format()?;
    format.fourcc = FourCC::new(b"MJPG");
    format.width = WIDTH;
    format.height = HEIGHT;
    let format = dev.set_format(&format)?;
    let mut params = dev.params()?;
    params.interval = Fraction::new(1, FPS);
    let params = dev.set_params(&params)?;
    check_format(&format, params.interval).map_err(io::Error::other)?;

    // Stream::drop panics when STREAMOFF fails with anything but ENODEV. Stop it by hand and
    // leak it on such a failure instead.
    let mut stream = ManuallyDrop::new(Stream::with_buffers(&dev, Type::VideoCapture, 4)?);
    // Only a safety net: the loop below polls first, so `next` never waits. (A timed-out `next`
    // would re-queue the same buffer on the following call, and the driver rejects that.)
    stream.set_timeout(Duration::from_secs(2));

    let result = pump(&dev, &mut stream, slot, status, stop, seq);
    match v4l::io::traits::Stream::stop(&mut *stream) {
        Ok(()) => unsafe { ManuallyDrop::drop(&mut stream) },
        Err(e) if e.raw_os_error() == Some(19) => unsafe { ManuallyDrop::drop(&mut stream) },
        Err(_) => {} // leaked on purpose: dropping it would panic
    }
    result
}

fn pump(
    dev: &Device,
    stream: &mut Stream,
    slot: &Slot,
    status: &Sender<Status>,
    stop: &AtomicBool,
    seq: &mut u64,
) -> io::Result<()> {
    // The first `next` queues every buffer and starts the stream.
    let mut newest = copy(stream.next()?);
    let _ = status.send(Status::Streaming);
    let mut last_decode: Option<Instant> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Drain every ready buffer, keep only the newest copy.
        while dev.handle().poll(libc_pollin(), 0)? > 0 {
            newest = copy(stream.next()?);
        }
        // Frames that arrive faster than DECODE_EVERY are dropped here, before any decode.
        if let Some(jpeg) = newest.take()
            && last_decode.is_none_or(|t| t.elapsed() >= DECODE_EVERY)
        {
            last_decode = Some(Instant::now());
            if let Ok(image) = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            {
                *seq += 1;
                let frame = Frame { seq: *seq, image };
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
            } // a bad JPEG is skipped
        }
        if dev.handle().poll(libc_pollin(), POLL_MS)? > 0 {
            newest = copy(stream.next()?);
        }
    }
}

fn copy((bytes, meta): (&[u8], &v4l::buffer::Metadata)) -> Option<Vec<u8>> {
    let used = (meta.bytesused as usize).min(bytes.len());
    Some(bytes[..used].to_vec())
}

const fn libc_pollin() -> i16 {
    0x001
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statuses_until_gone(capture: &Capture) -> Vec<Status> {
        let end = Instant::now() + Duration::from_secs(2);
        let mut seen = Vec::new();
        while Instant::now() < end {
            match capture.try_status() {
                Some(s) => {
                    let gone = matches!(s, Status::Gone(_));
                    seen.push(s);
                    if gone {
                        break;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        seen
    }

    #[test]
    fn each_capture_has_its_own_status_channel_and_slot() {
        let old = Capture::start(PathBuf::from("/nonexistent/old-video"), Shared::default());
        let new = Capture::start(PathBuf::from("/nonexistent/new-video"), Shared::default());
        // Each capture reports its own start and failure, exactly once, on its own channel.
        for capture in [&old, &new] {
            let seen = statuses_until_gone(capture);
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert_eq!(seen[0], Status::Starting);
            assert!(matches!(seen[1], Status::Gone(_)), "{seen:?}");
        }
        old.stop(Duration::from_millis(500));
        assert!(new.take_frame().is_none());
        new.stop(Duration::from_millis(500));
    }

    fn format(fourcc: &[u8; 4], w: u32, h: u32) -> Format {
        Format::new(w, h, FourCC::new(fourcc))
    }

    #[test]
    fn accepts_the_requested_mode() {
        assert_eq!(
            check_format(&format(b"MJPG", 960, 540), Fraction::new(1, 30)),
            Ok(())
        );
    }

    #[test]
    fn rejects_other_formats_sizes_and_rates() {
        assert!(check_format(&format(b"YUYV", 960, 540), Fraction::new(1, 30)).is_err());
        assert!(check_format(&format(b"MJPG", 1920, 1080), Fraction::new(1, 30)).is_err());
        assert!(check_format(&format(b"MJPG", 960, 540), Fraction::new(1, 60)).is_err());
    }

    #[test]
    fn rejects_a_zero_or_overflowing_interval() {
        let mjpg = format(b"MJPG", 960, 540);
        assert!(check_format(&mjpg, Fraction::new(0, 0)).is_err());
        assert!(check_format(&mjpg, Fraction::new(u32::MAX, 30)).is_err());
    }

    #[test]
    fn a_failed_spawn_reports_an_error() {
        let capture = Capture::start_with(
            |_| Err(io::Error::other("no threads")),
            PathBuf::from("/nonexistent/video"),
            Shared::default(),
        );
        assert_eq!(
            capture.try_status(),
            Some(Status::Error(
                "could not start the capture thread: no threads".into()
            ))
        );
    }

    #[test]
    fn busy_retries_do_not_repeat_starting() {
        fn busy_twice(
            _: &PathBuf,
            _: &Slot,
            _: &Sender<Status>,
            stop: &AtomicBool,
            seq: &mut u64,
        ) -> io::Result<()> {
            // `seq` counts the calls here: two EBUSY failures, then a stop request.
            *seq += 1;
            if *seq <= 2 {
                return Err(io::Error::from_raw_os_error(16));
            }
            stop.store(true, Ordering::Relaxed);
            Ok(())
        }
        let (status, statuses) = mpsc::channel();
        run(
            &PathBuf::from("/nonexistent/video"),
            &Slot::default(),
            &status,
            &Shared::default(),
            &AtomicBool::new(false),
            busy_twice,
            Duration::ZERO,
        );
        drop(status);
        let seen: Vec<Status> = statuses.iter().collect();
        assert_eq!(seen, [Status::Starting, Status::Busy, Status::Busy]);
    }

    #[test]
    fn only_busy_and_gone_are_retried() {
        assert!(retries(&Status::Busy));
        assert!(retries(&Status::Gone("ENODEV".into())));
        assert!(!retries(&Status::Error("EINVAL from S_FMT".into())));
        assert!(!retries(&Status::NoAccess("EACCES".into())));
    }

    #[test]
    fn classifies_errors() {
        assert_eq!(classify(&io::Error::from_raw_os_error(16)), Status::Busy);
        assert!(matches!(
            classify(&io::Error::from_raw_os_error(13)),
            Status::NoAccess(_)
        ));
        assert!(matches!(
            classify(&io::Error::from_raw_os_error(19)),
            Status::Gone(_)
        ));
        assert!(matches!(
            classify(&io::Error::from_raw_os_error(22)),
            Status::Error(_)
        ));
    }
}
