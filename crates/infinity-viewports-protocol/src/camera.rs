use crate::seqlock::Seqlock;

pub const CAMERA_MAGIC: u32 = u32::from_le_bytes(*b"ICAM");
pub const CAMERA_VERSION: u32 = 1;

/// camera attached to camera (moves with camera)
pub const MOUNT_FOLLOW: u32 = 0;
/// attached to aircraft
pub const MOUNT_AIRCRAFT: u32 = 1;

pub const APPLIED_FOLLOW: u32 = 0;
pub const APPLIED_PLACED: u32 = 1;
pub const APPLIED_PARENTED: u32 = 2;
pub const APPLIED_MISSING: u32 = 3;

/// [`CameraStatus::nose`]: the core's own scene view. A core from before
/// this field leaves it [`NOSE_UNKNOWN`].
pub const NOSE_UNKNOWN: u16 = 0;
/// Creating the view (or waiting for the sim to give it a slot).
pub const NOSE_CREATING: u16 = 1;
/// Rendering.
pub const NOSE_LIVE: u16 = 2;
/// The sim took the view away (or it failed); recreating after a backoff.
pub const NOSE_REACQUIRING: u16 = 3;
/// Given back for an unload; nothing more will be created.
pub const NOSE_STOPPED: u16 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CameraControl {
    pub magic: u32,
    pub version: u32,
    pub mount: u32,
    pub _pad: u32,
    pub right_m: f32,
    pub up_m: f32,
    pub forward_m: f32,
    /// Degrees added to the mount axes
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub roll_deg: f32,
    pub fov_deg: f32,
    pub near_m: f32,
    pub far_m: f32,
    pub aim_m: f32,
}

impl CameraControl {
    pub const fn stock() -> Self {
        Self {
            magic: CAMERA_MAGIC,
            version: CAMERA_VERSION,
            mount: MOUNT_FOLLOW,
            _pad: 0,
            right_m: 0.0,
            up_m: 0.0,
            forward_m: 6.0,
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            fov_deg: 24.0,
            near_m: 0.0,
            far_m: 0.0,
            aim_m: 0.0,
        }
    }

    pub fn valid(&self) -> bool {
        self.magic == CAMERA_MAGIC && self.version == CAMERA_VERSION
    }

    pub fn sanitized(mut self) -> Self {
        self.magic = CAMERA_MAGIC;
        self.version = CAMERA_VERSION;
        self.mount = if self.mount == MOUNT_AIRCRAFT {
            MOUNT_AIRCRAFT
        } else {
            MOUNT_FOLLOW
        };
        self._pad = 0;
        self.right_m = clamp(self.right_m, -500.0, 500.0);
        self.up_m = clamp(self.up_m, -500.0, 500.0);
        self.forward_m = clamp(self.forward_m, -500.0, 500.0);
        self.yaw_deg = clamp(self.yaw_deg, -180.0, 180.0);
        self.pitch_deg = clamp(self.pitch_deg, -89.0, 89.0);
        self.roll_deg = clamp(self.roll_deg, -180.0, 180.0);
        self.fov_deg = clamp(self.fov_deg, 1.0, 140.0);
        self.near_m = optional(self.near_m, 0.01, 20.0);
        self.far_m = optional(self.far_m, 10.0, 5_000_000.0);
        self.aim_m = optional(self.aim_m, 1.0, 100_000.0);
        self
    }
}

impl Default for CameraControl {
    fn default() -> Self {
        Self::stock()
    }
}

/// Viewport to host reporting
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CameraStatus {
    pub applied: u32,
    pub parented: u32,
    pub fov_deg: f32,
    pub near_m: f32,
    pub far_m: f32,
    /// `NOSE_*`: the nose view's state.
    pub nose: u16,
    /// Times the nose view was lost and recreated since injection
    /// (saturating).
    pub nose_losses: u16,
    pub origin: [f64; 3],
    pub note: [u8; 96],
}

impl CameraStatus {
    pub const fn empty() -> Self {
        Self {
            applied: APPLIED_FOLLOW,
            parented: 0,
            fov_deg: 0.0,
            near_m: 0.0,
            far_m: 0.0,
            nose: NOSE_UNKNOWN,
            nose_losses: 0,
            origin: [0.0; 3],
            note: [0; 96],
        }
    }

    pub fn set_note(&mut self, text: &str) {
        self.note = [0; 96];
        let bytes = text.as_bytes();
        let n = bytes.len().min(self.note.len().saturating_sub(1));
        self.note[..n].copy_from_slice(&bytes[..n]);
    }
}

impl Default for CameraStatus {
    fn default() -> Self {
        Self::empty()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AircraftPose {
    pub valid: u32,
    pub _pad: u32,
    pub origin: [f64; 3],
    pub right: [f64; 3],
    pub up: [f64; 3],
    pub forward: [f64; 3],
}

impl AircraftPose {
    pub const fn absent() -> Self {
        Self {
            valid: 0,
            _pad: 0,
            origin: [0.0; 3],
            right: [0.0; 3],
            up: [0.0; 3],
            forward: [0.0; 3],
        }
    }
}

impl Default for AircraftPose {
    fn default() -> Self {
        Self::absent()
    }
}

#[repr(C)]
pub struct PoseBlock {
    pub lock: Seqlock,
    pub pose: AircraftPose,
}

impl PoseBlock {
    pub const SIZE: u32 = core::mem::size_of::<PoseBlock>() as u32;
}

#[repr(C)]
pub struct CameraBlock {
    pub control_lock: Seqlock,
    pub control: CameraControl,
    pub status_lock: Seqlock,
    pub status: CameraStatus,
}

impl CameraBlock {
    pub const SIZE: u32 = core::mem::size_of::<CameraBlock>() as u32;
}

pub fn pose_block_name(pid: u32, buf: &mut [u8]) -> Option<usize> {
    named_block(b".pose", pid, buf)
}

pub fn camera_block_name(pid: u32, buf: &mut [u8]) -> Option<usize> {
    named_block(b".cam", pid, buf)
}

fn named_block(suffix: &[u8], pid: u32, buf: &mut [u8]) -> Option<usize> {
    const PREFIX: &[u8] = b"Local\\InfinityViewports.";
    if buf.len() < PREFIX.len() + suffix.len() + 1 {
        return None;
    }
    let mut n = 0;
    for &b in PREFIX {
        buf[n] = b;
        n += 1;
    }
    let mut tmp = [0u8; 10];
    let mut t = 0;
    let mut v = pid;
    if v == 0 {
        buf[n] = b'0';
        n += 1;
    } else {
        while v > 0 {
            tmp[t] = b'0' + (v % 10) as u8;
            t += 1;
            v /= 10;
        }
        while t > 0 {
            t -= 1;
            buf[n] = tmp[t];
            n += 1;
        }
    }
    for &b in suffix {
        if n >= buf.len() {
            return None;
        }
        buf[n] = b;
        n += 1;
    }
    Some(n)
}

fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if !v.is_finite() {
        return lo;
    }
    v.clamp(lo, hi)
}

fn optional(v: f32, lo: f32, hi: f32) -> f32 {
    if v == 0.0 {
        return 0.0;
    }
    clamp(v, lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_stock_round_trip() {
        let mut buf = [0u8; 64];
        let n = camera_block_name(45640, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"Local\\InfinityViewports.45640.cam");
        let n = pose_block_name(45640, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"Local\\InfinityViewports.45640.pose");
        assert_eq!(core::mem::size_of::<AircraftPose>(), 104);
        assert_eq!(core::mem::align_of::<AircraftPose>(), 8);
        assert_eq!(core::mem::offset_of!(PoseBlock, pose) % 8, 0);
        assert!(AircraftPose::absent().valid == 0);
        let stock = CameraControl::stock();
        assert!(stock.valid());
        let wild = CameraControl {
            magic: 1,
            version: 99,
            mount: 7,
            _pad: 3,
            right_m: f32::NAN,
            up_m: 10_000.0,
            forward_m: 6.0,
            yaw_deg: 0.0,
            pitch_deg: 120.0,
            roll_deg: 0.0,
            fov_deg: 0.0,
            near_m: 0.0,
            far_m: -4.0,
            aim_m: 0.0,
        };
        let clean = wild.sanitized();
        assert!(clean.valid());
        assert_eq!(clean.mount, MOUNT_FOLLOW);
        assert_eq!(clean.right_m, -500.0);
        assert_eq!(clean.up_m, 500.0);
        assert_eq!(clean.pitch_deg, 89.0);
        assert_eq!(clean.fov_deg, 1.0);
        assert_eq!(clean.near_m, 0.0);
        assert_eq!(clean.far_m, 10.0);
    }

    #[test]
    fn status_is_eight_aligned() {
        assert_eq!(core::mem::align_of::<CameraBlock>(), 8);
        assert_eq!(core::mem::offset_of!(CameraBlock, control), 4);
        assert_eq!(core::mem::offset_of!(CameraBlock, status) % 8, 0);
        // The nose fields took the old `_pad: u32`: same layout as before.
        assert_eq!(core::mem::offset_of!(CameraStatus, nose), 20);
        assert_eq!(core::mem::offset_of!(CameraStatus, origin), 24);
        assert_eq!(core::mem::size_of::<CameraStatus>(), 144);
        assert_eq!(
            core::mem::offset_of!(CameraBlock, status_lock) + core::mem::size_of::<Seqlock>(),
            core::mem::offset_of!(CameraBlock, status)
        );
    }
}
