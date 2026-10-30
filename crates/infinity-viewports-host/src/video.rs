use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::core::PCWSTR;

use infinity_viewports_protocol::{VIDEO_SLOTS, VideoBlock};

use crate::error::{HostError, Result};

pub struct VideoLayerData {
    pub role: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

pub struct VideoFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub layers: Vec<VideoLayerData>,
}

pub struct VideoReader {
    handle: HANDLE,
    ptr: *const VideoBlock,
    last_seq: u64,
}

unsafe impl Send for VideoReader {}
unsafe impl Sync for VideoReader {}

impl VideoReader {
    pub fn open(pid: u32) -> Result<VideoReader> {
        let mut name_buf = [0u8; 80];
        let n = infinity_viewports_protocol::video_name(pid, &mut name_buf)
            .ok_or(HostError::Os("name buffer".into()))?;
        let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
            .unwrap()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(wide.as_ptr()))
                .map_err(|_| HostError::NotConnected)?;
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, VideoBlock::SIZE);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return Err(HostError::NotConnected);
            }
            Ok(VideoReader {
                handle,
                ptr: view.Value as *const VideoBlock,
                last_seq: 0,
            })
        }
    }

    pub fn read_latest(&self) -> Option<VideoFrame> {
        read_block(unsafe { &*self.ptr })
    }

    pub fn read_new(&mut self) -> Option<VideoFrame> {
        let f = self.read_latest()?;
        if f.seq != self.last_seq {
            self.last_seq = f.seq;
            Some(f)
        } else {
            None
        }
    }
}

fn read_block(vb: &VideoBlock) -> Option<VideoFrame> {
    use core::sync::atomic::Ordering;
    for _ in 0..8 {
        let seq = vb.header.seq.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        let slot = ((seq - 1) % VIDEO_SLOTS as u64) as usize;
        let w = vb.header.width.load(Ordering::Relaxed);
        let h = vb.header.height.load(Ordering::Relaxed);
        let lc = (vb.header.layer_count.load(Ordering::Relaxed) as usize)
            .min(infinity_viewports_protocol::VIDEO_MAX_LAYERS);
        let mut layers = Vec::with_capacity(lc);
        let slot_base = unsafe {
            (vb.slots.as_ptr() as *const u8)
                .add(slot * infinity_viewports_protocol::VIDEO_SLOT_BYTES)
        };
        for i in 0..lc {
            let d = vb.header.descs[slot][i];
            let blen = d.byte_len as usize;
            if blen == 0 || d.offset as usize + blen > infinity_viewports_protocol::VIDEO_SLOT_BYTES
            {
                continue;
            }
            let mut bytes = vec![0u8; blen];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    slot_base.add(d.offset as usize),
                    bytes.as_mut_ptr(),
                    blen,
                )
            };
            layers.push(VideoLayerData {
                role: d.role,
                format: d.format,
                width: d.width,
                height: d.height,
                bytes,
            });
        }
        if vb.header.seq.load(Ordering::Acquire) == seq {
            return Some(VideoFrame {
                seq,
                width: w,
                height: h,
                layers,
            });
        }
    }
    None
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as *mut core::ffi::c_void,
            });
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use infinity_viewports_protocol::{
        VIDEO_LAYER_BYTES, VIDEO_MAGIC, VIDEO_SLOTS, VideoLayerDesc,
    };

    // A zeroed VideoBlock on the heap (via a byte buffer), cast like the mapping.
    fn blank() -> (Vec<u8>, *mut VideoBlock) {
        let mem = vec![0u8; VideoBlock::SIZE];
        let ptr = mem.as_ptr() as *mut VideoBlock;
        (mem, ptr)
    }

    // Publish one frame exactly as the bridge's `stream::write_frame` does.
    unsafe fn publish(vb: *mut VideoBlock, layers: &[(u32, u32, u32, u32, u8)]) {
        unsafe {
            let seq = (*vb).header.seq.load(Ordering::Relaxed);
            let wslot = (seq % VIDEO_SLOTS as u64) as usize;
            for (i, &(role, fmt, w, h, fill)) in layers.iter().enumerate() {
                let offset = i * VIDEO_LAYER_BYTES;
                let blen = (w * h) as usize * 4; // 4bpp test payload
                let dst = core::ptr::addr_of_mut!((*vb).slots[wslot]) as *mut u8;
                for b in 0..blen {
                    *dst.add(offset + b) = fill;
                }
                let d = VideoLayerDesc {
                    role,
                    format: fmt,
                    width: w,
                    height: h,
                    byte_len: blen as u32,
                    offset: offset as u32,
                };
                core::ptr::write(core::ptr::addr_of_mut!((*vb).header.descs[wslot][i]), d);
            }
            (*vb).header.magic.store(VIDEO_MAGIC, Ordering::Relaxed);
            (*vb)
                .header
                .layer_count
                .store(layers.len() as u32, Ordering::Relaxed);
            (*vb).header.seq.store(seq + 1, Ordering::Release);
        }
    }

    #[test]
    fn empty_block_reads_none() {
        let (_mem, vb) = blank();
        assert!(read_block(unsafe { &*vb }).is_none());
    }

    #[test]
    fn publish_then_read_roundtrips() {
        let (_mem, vb) = blank();
        unsafe { publish(vb, &[(0, 41, 8, 4, 0xAB), (2, 26, 8, 4, 0xCD)]) };
        let f = read_block(unsafe { &*vb }).expect("a frame");
        assert_eq!(f.seq, 1);
        assert_eq!(f.layers.len(), 2);
        assert_eq!((f.layers[0].role, f.layers[0].format), (0, 41));
        assert_eq!(f.layers[0].bytes.len(), 8 * 4 * 4);
        assert!(f.layers[0].bytes.iter().all(|&b| b == 0xAB));
        assert_eq!((f.layers[1].role, f.layers[1].format), (2, 26));
        assert!(f.layers[1].bytes.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn newest_of_three_slots_wins() {
        let (_mem, vb) = blank();
        for (n, fill) in [0x11u8, 0x22, 0x33, 0x44].into_iter().enumerate() {
            unsafe { publish(vb, &[(0, 41, 8, 4, fill)]) };
            let f = read_block(unsafe { &*vb }).unwrap();
            assert_eq!(f.seq, n as u64 + 1);
            assert!(f.layers[0].bytes.iter().all(|&b| b == fill));
        }
    }
}
