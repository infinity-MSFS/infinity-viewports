//! Creation of the host↔bridge shared-memory block, bridge side.
//!
//! The bridge creates the named file mapping (`Local\InfinityViewports.{pid}`,
//! keyed on its own — i.e. the sim's — pid) that the host opens with
//! `OpenFileMappingW`. The mapping is pagefile-backed and OS-zero-filled, so
//! every atomic, seqlock and ring in [`SharedBlock`] starts at its zero value,
//! which is the correct initial state; the service thread stamps the header
//! (`Header::publish`) only once it is ready to serve, after which the host may
//! read it.
//!
//! The mapping handle and view are intentionally leaked for the process
//! lifetime: the bridge never tears the block down while the host or any
//! recorded GPU work might still reference it — a deliberate leak is safer than
//! a use-after-free at teardown.

use std::iter::once;

use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::PCWSTR;

use infinity_viewports_protocol::{SharedBlock, VideoBlock};

/// A created, mapped shared block owned by the bridge.
pub struct Shared {
    pub ptr: *mut SharedBlock,
    pub pid: u32,
}

/// Create the shared block for this process. Returns `None` if the name could
/// not be formatted or the OS calls failed.
pub fn create() -> Option<Shared> {
    let pid = unsafe { GetCurrentProcessId() };
    let mut name_buf = [0u8; 64];
    let n = infinity_viewports_protocol::shared_name(pid, &mut name_buf)?;
    let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
        .ok()?
        .encode_utf16()
        .chain(once(0))
        .collect();

    unsafe {
        // Pagefile-backed section (INVALID_HANDLE_VALUE), read/write, named.
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            SharedBlock::SIZE,
            PCWSTR(wide.as_ptr()),
        )
        .ok()?;
        // Handle leaked intentionally (process-lifetime block).
        let view = MapViewOfFile(
            handle,
            FILE_MAP_ALL_ACCESS,
            0,
            0,
            SharedBlock::SIZE as usize,
        );
        if view.Value.is_null() {
            return None;
        }
        Some(Shared {
            ptr: view.Value as *mut SharedBlock,
            pid,
        })
    }
}

/// Create the larger, on-demand **video** mapping (`Local\InfinityViewports.{pid}
/// .vid`) for the live stream and return its base pointer. Pagefile-backed and
/// OS-zero-filled, so the `VideoBlock` header starts zeroed (`seq == 0` = no
/// frame yet). Handle leaked for the process lifetime (the host may read it, and
/// the stream may write it, until the sim exits).
pub fn create_video(pid: u32) -> Option<usize> {
    let mut name_buf = [0u8; 80];
    let n = infinity_viewports_protocol::video_name(pid, &mut name_buf)?;
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
            VideoBlock::SIZE as u32,
            PCWSTR(wide.as_ptr()),
        )
        .ok()?;
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, VideoBlock::SIZE);
        if view.Value.is_null() {
            return None;
        }
        Some(view.Value as usize)
    }
}
