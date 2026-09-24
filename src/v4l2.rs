//! `auto_exposure` and `brightness` through normal V4L2 controls, on a control-only fd.

use std::{io, path::Path};

use v4l::{
    Device,
    control::{Control, MenuItem, Value},
};

use crate::camera::Mode;

const V4L2_CID_EXPOSURE_AUTO: u32 = 0x009a_0901;
const V4L2_CID_BRIGHTNESS: u32 = 0x0098_0900;

pub struct V4l2Controls {
    dev: Device,
    auto_index: u32,
    shutter_index: u32,
    brightness_range: (i64, i64),
}

impl V4l2Controls {
    pub fn open(video_node: &Path) -> io::Result<Self> {
        let dev = Device::with_path(video_node)?;
        let controls = dev.query_controls()?;

        let mode = controls
            .iter()
            .find(|c| c.id == V4L2_CID_EXPOSURE_AUTO)
            .ok_or_else(|| other("camera has no auto_exposure control"))?;
        let items: Vec<(u32, String)> = mode
            .items
            .iter()
            .flatten()
            .filter_map(|(i, m)| match m {
                MenuItem::Name(n) => Some((*i, n.clone())),
                MenuItem::Value(_) => None,
            })
            .collect();
        let (auto_index, shutter_index) = mode_indices(&items).ok_or_else(|| {
            other(format!(
                "auto_exposure menu has no Auto/Shutter item: {items:?}"
            ))
        })?;

        let brightness = controls
            .iter()
            .find(|c| c.id == V4L2_CID_BRIGHTNESS)
            .ok_or_else(|| other("camera has no brightness control"))?;
        let brightness_range = (brightness.minimum, brightness.maximum);

        Ok(Self {
            dev,
            auto_index,
            shutter_index,
            brightness_range,
        })
    }

    pub fn brightness_range(&self) -> (i64, i64) {
        self.brightness_range
    }

    pub fn mode(&self) -> io::Result<Mode> {
        match self.dev.control(V4L2_CID_EXPOSURE_AUTO)?.value {
            Value::Integer(v) if v == i64::from(self.auto_index) => Ok(Mode::Auto),
            Value::Integer(v) if v == i64::from(self.shutter_index) => Ok(Mode::Shutter),
            other_value => Err(other(format!(
                "unexpected auto_exposure value {other_value:?}"
            ))),
        }
    }

    pub fn set_mode(&self, mode: Mode) -> io::Result<()> {
        let index = match mode {
            Mode::Auto => self.auto_index,
            Mode::Shutter => self.shutter_index,
        };
        self.dev.set_control(Control {
            id: V4L2_CID_EXPOSURE_AUTO,
            value: Value::Integer(i64::from(index)),
        })
    }

    pub fn brightness(&self) -> io::Result<i64> {
        match self.dev.control(V4L2_CID_BRIGHTNESS)?.value {
            Value::Integer(v) => Ok(v),
            other_value => Err(other(format!(
                "unexpected brightness value {other_value:?}"
            ))),
        }
    }

    pub fn set_brightness(&self, value: i64) -> io::Result<()> {
        self.dev.set_control(Control {
            id: V4L2_CID_BRIGHTNESS,
            value: Value::Integer(value),
        })
    }
}

/// Menu indices of "Auto Mode" and "Shutter Priority Mode", matched by name, never assumed.
fn mode_indices(items: &[(u32, String)]) -> Option<(u32, u32)> {
    let find = |name: &str| items.iter().find(|(_, n)| n == name).map(|(i, _)| *i);
    Some((find("Auto Mode")?, find("Shutter Priority Mode")?))
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(pairs: &[(u32, &str)]) -> Vec<(u32, String)> {
        pairs.iter().map(|(i, n)| (*i, n.to_string())).collect()
    }

    #[test]
    fn mode_indices_come_from_names() {
        let facecam = items(&[(0, "Auto Mode"), (2, "Shutter Priority Mode")]);
        assert_eq!(mode_indices(&facecam), Some((0, 2)));

        let reordered = items(&[(3, "Shutter Priority Mode"), (1, "Auto Mode")]);
        assert_eq!(mode_indices(&reordered), Some((1, 3)));
    }

    #[test]
    fn missing_mode_name_is_none() {
        assert_eq!(
            mode_indices(&items(&[(0, "Auto Mode"), (1, "Manual Mode")])),
            None
        );
        assert_eq!(mode_indices(&[]), None);
    }

    /// `cargo test -- --ignored`: pins the indices the real camera reports.
    #[test]
    #[ignore = "needs a connected Facecam"]
    fn hardware_mode_indices() {
        let found = crate::usb::discover(Path::new("/sys"), Path::new("/dev")).expect("no Facecam");
        let controls = V4l2Controls::open(&found.video_node.expect("no video node")).unwrap();
        assert_eq!((controls.auto_index, controls.shutter_index), (0, 2));
    }
}
