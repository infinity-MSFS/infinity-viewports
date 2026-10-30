use std::iter::once;

use infinity_viewports_protocol::{
    AircraftPose, CameraBlock, CameraControl, CameraStatus, PoseBlock,
};
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::PCWSTR;

pub struct Share {
    ptr: usize,
    pose: usize,
    last: CameraControl,
}

unsafe impl Send for Share {}

impl Share {
    pub fn create() -> Share {
        let ptr = open_mapping(
            infinity_viewports_protocol::camera_block_name,
            CameraBlock::SIZE,
        )
        .unwrap_or(0);
        let pose = open_mapping(
            infinity_viewports_protocol::pose_block_name,
            PoseBlock::SIZE,
        )
        .unwrap_or(0);
        Share {
            ptr,
            pose,
            last: CameraControl::stock(),
        }
    }

    pub fn read(&mut self) -> CameraControl {
        let Some(block) = self.block() else {
            return self.last;
        };
        let value = unsafe {
            let payload = core::ptr::addr_of!((*block).control);
            (*block).control_lock.read(payload, 8)
        };
        if let Some(value) = value.filter(|c| c.valid()) {
            self.last = value.sanitized();
        }
        self.last
    }

    pub fn read_pose(&self) -> AircraftPose {
        if self.pose == 0 {
            return AircraftPose::absent();
        }
        let block = self.pose as *const PoseBlock;
        unsafe {
            let payload = core::ptr::addr_of!((*block).pose);
            (*block)
                .lock
                .read(payload, 8)
                .unwrap_or_else(AircraftPose::absent)
        }
    }

    pub fn publish(&self, status: CameraStatus) {
        let Some(block) = self.block() else {
            return;
        };
        unsafe {
            let payload = core::ptr::addr_of_mut!((*block).status);
            (*block).status_lock.write(payload, status);
        }
    }

    fn block(&self) -> Option<*mut CameraBlock> {
        (self.ptr != 0).then_some(self.ptr as *mut CameraBlock)
    }
}

fn open_mapping(name_of: fn(u32, &mut [u8]) -> Option<usize>, size: u32) -> Option<usize> {
    let pid = unsafe { GetCurrentProcessId() };
    let mut name_buf = [0u8; 80];
    let n = name_of(pid, &mut name_buf)?;
    let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
        .ok()?
        .encode_utf16()
        .chain(once(0))
        .collect();
    unsafe {
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size,
            PCWSTR(wide.as_ptr()),
        )
        .ok()?;
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size as usize);
        if view.Value.is_null() {
            return None;
        }
        Some(view.Value as usize)
    }
}
