use std::ptr;

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

use infinity_viewports_protocol::{
    AircraftPose, CameraBlock, CameraControl, CameraStatus, PoseBlock,
};

use crate::error::{HostError, Result};

/// Mapped nose-camera block. Drop unmaps this process's view.
pub struct CameraLink {
    handle: HANDLE,
    ptr: *mut CameraBlock,
    pose_handle: HANDLE,
    pose_ptr: *mut PoseBlock,
}

// The pointer targets a process-shared mapping. The seqlocks are the
// synchronisation, so the handle is Send + Sync for the tuner's threads.
unsafe impl Send for CameraLink {}
unsafe impl Sync for CameraLink {}

impl CameraLink {
    /// Create the named block, or open it if the nose driver already did.
    pub fn open(pid: u32) -> Result<CameraLink> {
        let mut name_buf = [0u8; 64];
        let n = infinity_viewports_protocol::camera_block_name(pid, &mut name_buf)
            .ok_or_else(|| HostError::Os("name buffer".into()))?;
        let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
            .map_err(|_| HostError::Os("camera name".into()))?
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                CameraBlock::SIZE,
                PCWSTR(wide.as_ptr()),
            )
            .map_err(|e| HostError::Os(format!("camera mapping: {e}")))?;
            let view = MapViewOfFile(
                handle,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                CameraBlock::SIZE as usize,
            );
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return Err(HostError::NotConnected);
            }
            let (pose_handle, pose_ptr) =
                open_pose(pid).unwrap_or((HANDLE::default(), ptr::null_mut()));
            Ok(CameraLink {
                handle,
                ptr: view.Value as *mut CameraBlock,
                pose_handle,
                pose_ptr,
            })
        }
    }

    /// Publish one control snapshot. Values are clamped by [`CameraControl::sanitized`].
    pub fn write_control(&self, control: CameraControl) -> Result<()> {
        let value = control.sanitized();
        unsafe {
            let payload = core::ptr::addr_of_mut!((*self.ptr).control);
            (*self.ptr).control_lock.write(payload, value);
        }
        Ok(())
    }

    /// Publish the aircraft datum. A missing pose mapping leaves the nose with
    /// no absolute basis; the slider block is unaffected.
    pub fn write_pose(&self, pose: AircraftPose) -> Result<()> {
        if self.pose_ptr.is_null() {
            return Err(HostError::NotConnected);
        }
        unsafe {
            let payload = core::ptr::addr_of_mut!((*self.pose_ptr).pose);
            (*self.pose_ptr).lock.write(payload, pose);
        }
        Ok(())
    }

    /// Latest control snapshot. `valid()` is false until something has been written.
    pub fn read_control(&self) -> Result<CameraControl> {
        unsafe {
            let payload = core::ptr::addr_of!((*self.ptr).control);
            (*self.ptr)
                .control_lock
                .read(payload, 8)
                .ok_or(HostError::Timeout)
        }
    }

    /// Latest status snapshot published by the nose driver.
    pub fn read_status(&self) -> Result<CameraStatus> {
        unsafe {
            let payload = core::ptr::addr_of!((*self.ptr).status);
            (*self.ptr)
                .status_lock
                .read(payload, 8)
                .ok_or(HostError::Timeout)
        }
    }
}

impl Drop for CameraLink {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS;
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as *mut _,
            });
            let _ = CloseHandle(self.handle);
            if !self.pose_ptr.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.pose_ptr as *mut _,
                });
                let _ = CloseHandle(self.pose_handle);
            }
        }
    }
}

fn open_pose(pid: u32) -> Result<(HANDLE, *mut PoseBlock)> {
    let mut name_buf = [0u8; 64];
    let n = infinity_viewports_protocol::pose_block_name(pid, &mut name_buf)
        .ok_or_else(|| HostError::Os("pose name buffer".into()))?;
    let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
        .map_err(|_| HostError::Os("pose name".into()))?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            PoseBlock::SIZE,
            PCWSTR(wide.as_ptr()),
        )
        .map_err(|e| HostError::Os(format!("pose mapping: {e}")))?;
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, PoseBlock::SIZE as usize);
        if view.Value.is_null() {
            let _ = CloseHandle(handle);
            return Err(HostError::NotConnected);
        }
        Ok((handle, view.Value as *mut PoseBlock))
    }
}
