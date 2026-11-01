#![no_std]

mod airframe;
mod camera;
mod command;
mod header;
mod seqlock;
mod status;
mod video;

pub use airframe::{ECEF_MATCH_M, EcefFit, EcefMap, fit_ecef, pose_populated};
pub use camera::*;
pub use command::*;
pub use header::*;
pub use seqlock::Seqlock;
pub use status::*;
pub use video::*;

pub const PROTOCOL_VERSION_MAJOR: u16 = 2;
pub const PROTOCOL_VERSION_MINOR: u16 = 0;
pub const SHARED_MAGIC: u32 = u32::from_le_bytes(*b"INFV");
pub const SHARED_NAME_PREFIX: &str = "Local\\InfinityViewports";

/// get the shared object name for a given PID
pub fn shared_name(pid: u32, buf: &mut [u8]) -> Option<usize> {
    let mut n = 0;
    let mut put = |b: u8, buf: &mut [u8]| -> bool {
        if n < buf.len() {
            buf[n] = b;
            n += 1;
            true
        } else {
            false
        }
    };
    for &b in SHARED_NAME_PREFIX.as_bytes() {
        if !put(b, buf) {
            return None;
        }
    }
    if !put(b'.', buf) {
        return None;
    }
    let mut tmp = [0u8; 10];
    let mut t = 0;
    let mut v = pid;
    if v == 0 {
        if !put(b'0', buf) {
            return None;
        }
    } else {
        while v > 0 {
            tmp[t] = b'0' + (v % 10) as u8;
            t += 1;
            v /= 10;
        }
        while t > 0 {
            t -= 1;
            if !put(tmp[t], buf) {
                return None;
            }
        }
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_formats() {
        let mut buf = [0u8; 64];
        let n = shared_name(12345, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"Local\\InfinityViewports.12345");
        let n = shared_name(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"Local\\InfinityViewports.0");
    }

    #[test]
    fn name_too_small() {
        let mut buf = [0u8; 4];
        assert_eq!(shared_name(1, &mut buf), None);
    }
}
