//! Facecam discovery through sysfs, and the exposure-time control over usbfs.
//!
//! uvcvideo only lets `exposure_time_absolute` be written when `auto_exposure` is Manual, a mode
//! the Facecam does not have, so the V4L2 control is always locked. The camera itself accepts
//! `CT_EXPOSURE_TIME_ABSOLUTE` in Shutter Priority, so the request goes straight to the device.
//! It uses the DEVICE recipient: usbfs passes a device-recipient class request without claiming
//! interface 0, which uvcvideo owns, so it works while the camera streams.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub const VENDOR_ID: u16 = 0x0fd9;
pub const PRODUCT_ID: u16 = 0x0078;

/// Camera terminal unit id and the video-control interface (from the USB descriptors).
const CAMERA_TERMINAL: u16 = 1;
const VIDEO_CONTROL_INTERFACE: u16 = 0;
const CT_EXPOSURE_TIME_ABSOLUTE: u16 = 0x04;
const TIMEOUT_MS: u32 = 500;

/// A Facecam found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// sysfs name of the USB device, e.g. `6-3`.
    pub sysname: String,
    /// `/dev/bus/usb/BBB/DDD`.
    pub usb_node: PathBuf,
    /// `/dev/videoN` of the capture node (`index == 0`), if the driver has bound.
    pub video_node: Option<PathBuf>,
}

/// Finds the first Facecam, in sorted sysfs-name order.
///
/// `sysfs` is normally `/sys` and `dev` is normally `/dev`; tests pass fixture directories.
pub fn discover(sysfs: &Path, dev: &Path) -> Option<Found> {
    let mut devices = sorted_entries(&sysfs.join("bus/usb/devices"));
    devices.retain(|d| read_hex(&d.join("idVendor")) == Some(VENDOR_ID));
    devices.retain(|d| read_hex(&d.join("idProduct")) == Some(PRODUCT_ID));
    let device = devices.into_iter().next()?;

    let bus = read_u32(&device.join("busnum"))?;
    let num = read_u32(&device.join("devnum"))?;
    let usb_node = dev.join(format!("bus/usb/{bus:03}/{num:03}"));
    let sysname = device.file_name()?.to_string_lossy().into_owned();
    let video_node = video_node_for(sysfs, dev, &device);
    Some(Found {
        sysname,
        usb_node,
        video_node,
    })
}

/// The capture node of a USB device. A video node's real path is under the USB *interface*
/// (`.../6-3/6-3:1.0/video4linux/video0`), which is itself under the device directory.
fn video_node_for(sysfs: &Path, dev: &Path, device: &Path) -> Option<PathBuf> {
    let device = fs::canonicalize(device).ok()?;
    sorted_entries(&sysfs.join("class/video4linux"))
        .into_iter()
        .filter(|v| fs::read_to_string(v.join("index")).is_ok_and(|s| s.trim() == "0"))
        .find(|v| fs::canonicalize(v).is_ok_and(|real| real.starts_with(&device)))
        .and_then(|v| v.file_name().map(|n| dev.join(n)))
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

fn read_hex(path: &Path) -> Option<u16> {
    u16::from_str_radix(fs::read_to_string(path).ok()?.trim(), 16).ok()
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// UVC requests used on the exposure control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    SetCur,
    GetCur,
    GetMin,
    GetMax,
}

/// The fields of a USB control setup packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// Builds the setup packet: class request, DEVICE recipient (0x20 out, 0xA0 in).
pub fn setup(request: Request) -> Setup {
    let (request_type, request) = match request {
        Request::SetCur => (0x20, 0x01),
        Request::GetCur => (0xA0, 0x81),
        Request::GetMin => (0xA0, 0x82),
        Request::GetMax => (0xA0, 0x83),
    };
    Setup {
        request_type,
        request,
        value: CT_EXPOSURE_TIME_ABSOLUTE << 8,
        index: (CAMERA_TERMINAL << 8) | VIDEO_CONTROL_INTERFACE,
        length: 4,
    }
}

/// `struct usbdevfs_ctrltransfer` from `<linux/usbdevice_fs.h>`.
#[repr(C)]
pub struct CtrlTransfer {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub timeout: u32,
    pub data: *mut u8,
}

nix::ioctl_readwrite!(usbdevfs_control, b'U', 0, CtrlTransfer);

/// The exposure-time control of one Facecam, through its usbfs node.
pub struct UvcExposure {
    file: File,
}

impl UvcExposure {
    pub fn open(usb_node: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(usb_node)?;
        Ok(Self { file })
    }

    /// Exposure time in units of 100 µs.
    pub fn get(&self, request: Request) -> io::Result<u32> {
        self.transfer(request, 0)
    }

    pub fn set(&self, value: u32) -> io::Result<()> {
        self.transfer(Request::SetCur, value).map(|_| ())
    }

    fn transfer(&self, request: Request, value: u32) -> io::Result<u32> {
        let s = setup(request);
        let mut data = value.to_le_bytes();
        let mut transfer = CtrlTransfer {
            request_type: s.request_type,
            request: s.request,
            value: s.value,
            index: s.index,
            length: s.length,
            timeout: TIMEOUT_MS,
            data: data.as_mut_ptr(),
        };
        // SAFETY: `transfer` is a valid usbdevfs_ctrltransfer whose `data` points at 4 bytes
        // that outlive the call, matching `length`.
        unsafe { usbdevfs_control(self.file.as_raw_fd(), &mut transfer) }
            .map_err(io::Error::from)?;
        Ok(u32::from_le_bytes(data))
    }
}

#[cfg(test)]
mod tests {
    use std::{mem, os::unix::fs::symlink};

    use super::*;

    #[test]
    fn setup_bytes_are_locked() {
        let cases = [
            (Request::SetCur, 0x20, 0x01),
            (Request::GetCur, 0xA0, 0x81),
            (Request::GetMin, 0xA0, 0x82),
            (Request::GetMax, 0xA0, 0x83),
        ];
        for (request, request_type, code) in cases {
            let s = setup(request);
            assert_eq!(s.request_type, request_type, "{request:?}");
            assert_eq!(s.request, code, "{request:?}");
            assert_eq!(s.value, 0x0400);
            assert_eq!(s.index, 0x0100);
            assert_eq!(s.length, 4);
        }
    }

    #[test]
    fn ctrl_transfer_layout_matches_the_kernel() {
        assert_eq!(mem::size_of::<CtrlTransfer>(), 24);
        assert_eq!(mem::offset_of!(CtrlTransfer, length), 6);
        assert_eq!(mem::offset_of!(CtrlTransfer, timeout), 8);
        assert_eq!(mem::offset_of!(CtrlTransfer, data), 16);
    }

    #[test]
    fn payload_is_u32_little_endian() {
        assert_eq!(200u32.to_le_bytes(), [0xC8, 0, 0, 0]);
        assert_eq!(u32::from_le_bytes([0xC4, 0x09, 0, 0]), 2500);
    }

    /// Builds a sysfs tree shaped like the real one:
    /// devices/usb6/<name>/<name>:1.0/video4linux/videoN, with the class and bus links.
    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            for d in ["sys/bus/usb/devices", "sys/class/video4linux", "dev"] {
                fs::create_dir_all(root.path().join(d)).unwrap();
            }
            Self { root }
        }

        fn sys(&self) -> PathBuf {
            self.root.path().join("sys")
        }

        fn dev(&self) -> PathBuf {
            self.root.path().join("dev")
        }

        fn add_usb(&self, name: &str, vid: &str, pid: &str, bus: u32, num: u32) {
            let dir = self.sys().join("devices/usb6").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("idVendor"), format!("{vid}\n")).unwrap();
            fs::write(dir.join("idProduct"), format!("{pid}\n")).unwrap();
            fs::write(dir.join("busnum"), format!("{bus}\n")).unwrap();
            fs::write(dir.join("devnum"), format!("{num}\n")).unwrap();
            symlink(&dir, self.sys().join("bus/usb/devices").join(name)).unwrap();
        }

        fn add_video(&self, usb: &str, video: &str, index: u32) {
            let dir = self
                .sys()
                .join("devices/usb6")
                .join(usb)
                .join(format!("{usb}:1.0/video4linux"))
                .join(video);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("index"), format!("{index}\n")).unwrap();
            symlink(&dir, self.sys().join("class/video4linux").join(video)).unwrap();
        }
    }

    #[test]
    fn finds_the_facecam_and_its_capture_node() {
        let f = Fixture::new();
        f.add_usb("5-1", "0fd9", "0063", 5, 22); // Stream Deck: same vendor, other product
        f.add_usb("6-3", "0fd9", "0078", 6, 7);
        f.add_video("6-3", "video1", 1); // metadata node
        f.add_video("6-3", "video0", 0);

        let found = discover(&f.sys(), &f.dev()).unwrap();
        assert_eq!(found.sysname, "6-3");
        assert_eq!(found.usb_node, f.dev().join("bus/usb/006/007"));
        assert_eq!(found.video_node, Some(f.dev().join("video0")));
    }

    #[test]
    fn ignores_video_nodes_of_other_devices() {
        let f = Fixture::new();
        f.add_usb("1-1", "046d", "085e", 1, 2);
        f.add_video("1-1", "video0", 0);
        f.add_usb("6-3", "0fd9", "0078", 6, 7);
        f.add_video("6-3", "video2", 0);

        let found = discover(&f.sys(), &f.dev()).unwrap();
        assert_eq!(found.video_node, Some(f.dev().join("video2")));
    }

    #[test]
    fn two_facecams_pick_the_first_by_sysfs_name() {
        let f = Fixture::new();
        f.add_usb("6-4", "0fd9", "0078", 6, 9);
        f.add_usb("6-3", "0fd9", "0078", 6, 7);
        assert_eq!(discover(&f.sys(), &f.dev()).unwrap().sysname, "6-3");
    }

    #[test]
    fn no_facecam_is_none_and_unbound_video_is_none() {
        let f = Fixture::new();
        assert_eq!(discover(&f.sys(), &f.dev()), None);
        f.add_usb("6-3", "0fd9", "0078", 6, 7);
        assert_eq!(discover(&f.sys(), &f.dev()).unwrap().video_node, None);
    }

    /// Runs against the real camera: `cargo test -- --ignored`. Restores the exposure it found.
    #[test]
    #[ignore = "needs a connected Facecam"]
    fn hardware_set_and_read_back() {
        struct Restore<'a>(&'a UvcExposure, u32);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                if let Err(e) = self.0.set(self.1) {
                    eprintln!("could not restore exposure {}: {e}", self.1);
                }
            }
        }

        let found = discover(Path::new("/sys"), Path::new("/dev")).expect("no Facecam");
        let cam = UvcExposure::open(&found.usb_node).unwrap();
        let saved = cam.get(Request::GetCur).unwrap();
        let _restore = Restore(&cam, saved);
        assert_eq!(cam.get(Request::GetMin).unwrap(), 1);
        assert_eq!(cam.get(Request::GetMax).unwrap(), 2500);
        let probe = if saved == 200 { 300 } else { 200 };
        cam.set(probe).unwrap();
        assert_eq!(cam.get(Request::GetCur).unwrap(), probe);
    }
}
