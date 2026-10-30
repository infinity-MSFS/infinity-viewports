use core::sync::atomic::{AtomicU32, AtomicU64};

pub const VIDEO_MAGIC: u32 = u32::from_le_bytes(*b"INFV");
pub const VIDEO_VERSION: u32 = 1;

/// defaulted to MD-10 flir cam
pub const VIDEO_MAX_W: u32 = 1024;
pub const VIDEO_MAX_H: u32 = 576;
pub const VIDEO_MAX_BPP: usize = 8; // stream is R16G16B16A16_FLOAT colors or 8 bytes
pub const VIDEO_LAYER_BYTES: usize = VIDEO_MAX_W as usize * VIDEO_MAX_H as usize * VIDEO_MAX_BPP;
pub const VIDEO_MAX_LAYERS: usize = 4;
pub const VIDEO_SLOTS: usize = 3;
pub const VIDEO_SLOT_BYTES: usize = VIDEO_LAYER_BYTES * VIDEO_MAX_LAYERS;
pub const VIDEO_NAME_SUFFIX: &str = ".vid";

pub fn video_name(pid: u32, buf: &mut [u8]) -> Option<usize> {
    let n = crate::shared_name(pid, buf)?;
    let suffix = VIDEO_NAME_SUFFIX.as_bytes();
    if n + suffix.len() > buf.len() {
        return None;
    }
    buf[n..n + suffix.len()].copy_from_slice(suffix);
    Some(n + suffix.len())
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoLayerDesc {
    pub role: u32,
    /// DXGI format number of the source
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub byte_len: u32,
    pub offset: u32,
}

impl VideoLayerDesc {
    pub const ZERO: VideoLayerDesc = VideoLayerDesc {
        role: 0,
        format: 0,
        width: 0,
        height: 0,
        byte_len: 0,
        offset: 0,
    };
}
#[repr(C)]
pub struct VideoHeader {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub seq: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub layer_count: AtomicU32,
    pub _pad0: AtomicU32,
    pub _pad1: AtomicU64,
    pub descs: [[VideoLayerDesc; VIDEO_MAX_LAYERS]; VIDEO_SLOTS],
}

#[repr(C)]
pub struct VideoBlock {
    pub header: VideoHeader,
    pub slots: [[u8; VIDEO_SLOT_BYTES]; VIDEO_SLOTS],
}

impl VideoBlock {
    pub const SIZE: usize = core::mem::size_of::<VideoBlock>();
}
