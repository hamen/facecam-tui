//! The operations the control worker needs, behind a trait so its ordering is unit-tested.

use std::{io, path::Path};

use crate::{
    usb::{Request, UvcExposure},
    v4l2::V4l2Controls,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Shutter,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Mode::Auto => Mode::Shutter,
            Mode::Shutter => Mode::Auto,
        }
    }
}

pub trait Camera {
    fn exposure(&mut self) -> io::Result<u32>;
    fn exposure_range(&mut self) -> io::Result<(u32, u32)>;
    fn set_exposure(&mut self, value: u32) -> io::Result<()>;
    fn mode(&mut self) -> io::Result<Mode>;
    fn set_mode(&mut self, mode: Mode) -> io::Result<()>;
    fn brightness(&mut self) -> io::Result<i64>;
    fn brightness_range(&mut self) -> io::Result<(i64, i64)>;
    fn set_brightness(&mut self, value: i64) -> io::Result<()>;
}

/// The real Facecam: exposure over usbfs, mode and brightness over V4L2.
///
/// A usbfs node that cannot be opened (EACCES without the 0666 udev rule) does not stop the
/// other controls: exposure calls return that error, mode and brightness keep working.
pub struct Facecam {
    exposure: Result<UvcExposure, String>,
    v4l2: V4l2Controls,
}

impl Facecam {
    pub fn open(usb_node: &Path, video_node: &Path) -> io::Result<Self> {
        let exposure = UvcExposure::open(usb_node).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => format!(
                "no write access to {} (needs the 0fd9 MODE=0666 udev rule)",
                usb_node.display()
            ),
            _ => format!("{}: {e}", usb_node.display()),
        });
        Ok(Self {
            exposure,
            v4l2: V4l2Controls::open(video_node)?,
        })
    }

    fn uvc(&self) -> io::Result<&UvcExposure> {
        self.exposure
            .as_ref()
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.clone()))
    }
}

impl Camera for Facecam {
    fn exposure(&mut self) -> io::Result<u32> {
        self.uvc()?.get(Request::GetCur)
    }

    fn exposure_range(&mut self) -> io::Result<(u32, u32)> {
        let uvc = self.uvc()?;
        let (min, max) = (uvc.get(Request::GetMin)?, uvc.get(Request::GetMax)?);
        if min > max {
            return Err(io::Error::other(format!(
                "camera reports exposure range {min} > {max}"
            )));
        }
        Ok((min, max))
    }

    fn set_exposure(&mut self, value: u32) -> io::Result<()> {
        self.uvc()?.set(value)
    }

    fn mode(&mut self) -> io::Result<Mode> {
        self.v4l2.mode()
    }

    fn set_mode(&mut self, mode: Mode) -> io::Result<()> {
        self.v4l2.set_mode(mode)
    }

    fn brightness(&mut self) -> io::Result<i64> {
        self.v4l2.brightness()
    }

    fn brightness_range(&mut self) -> io::Result<(i64, i64)> {
        Ok(self.v4l2.brightness_range())
    }

    fn set_brightness(&mut self, value: i64) -> io::Result<()> {
        self.v4l2.set_brightness(value)
    }
}
