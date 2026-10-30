use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::core::PCWSTR;

use infinity_viewports_protocol::{
    CommandCode, CommandSlot, ConfigSnapshot, HeaderReject, LogRecord, SharedBlock, StatusSnapshot,
};

use crate::error::{HostError, Result};

pub struct Channel {
    handle: HANDLE,
    ptr: *mut SharedBlock,
    config: Mutex<ConfigSnapshot>,
    log_cursor: Mutex<u64>,
}

unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

impl Channel {
    pub fn open(pid: u32) -> Result<Channel> {
        let mut name_buf = [0u8; 64];
        let n = infinity_viewports_protocol::shared_name(pid, &mut name_buf)
            .ok_or(HostError::Os("name buffer".into()))?;
        let wide: Vec<u16> = std::str::from_utf8(&name_buf[..n])
            .unwrap()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(wide.as_ptr()))
                .map_err(|_| HostError::NotConnected)?;
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<SharedBlock>());
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return Err(HostError::NotConnected);
            }
            let ptr = view.Value as *mut SharedBlock;
            let block = &*ptr;
            if let Some(reason) = block.header.validate(SharedBlock::SIZE) {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
                return Err(reject_to_err(reason));
            }
            let config = block.config.snapshot;
            Ok(Channel {
                handle,
                ptr,
                config: Mutex::new(config),
                log_cursor: Mutex::new(0),
            })
        }
    }

    fn block(&self) -> &SharedBlock {
        unsafe { &*self.ptr }
    }

    pub fn verify_protocol(&self) -> Result<()> {
        match self.block().header.validate(SharedBlock::SIZE) {
            None => Ok(()),
            Some(r) => Err(reject_to_err(r)),
        }
    }

    pub fn update_config(&self, edit: impl FnOnce(&mut ConfigSnapshot)) -> Result<()> {
        let mut mirror = self.config.lock().unwrap();
        edit(&mut mirror);
        mirror.generation = mirror.generation.wrapping_add(1);
        let snapshot = *mirror;
        unsafe {
            let payload = core::ptr::addr_of_mut!((*self.ptr).config.snapshot);
            (*self.ptr).config.lock.write(payload, snapshot);
        }
        Ok(())
    }

    pub fn read_status(&self) -> Result<StatusSnapshot> {
        unsafe {
            let payload = core::ptr::addr_of!((*self.ptr).status.snapshot);
            (*self.ptr)
                .status
                .lock
                .read(payload, 16)
                .ok_or(HostError::Timeout)
        }
    }

    pub fn send_command(&self, code: CommandCode, arg: &[u8]) -> Result<()> {
        let mut slot = CommandSlot::EMPTY;
        slot.code = code as u32;
        let n = arg.len().min(slot.arg.len());
        slot.arg[..n].copy_from_slice(&arg[..n]);
        if self.block().commands.push(slot) {
            Ok(())
        } else {
            Err(HostError::Timeout)
        }
    }

    pub fn dump_capture(&self, path: &str) -> Result<()> {
        self.send_command(CommandCode::DumpCapture, path.as_bytes())
    }

    pub fn drain_logs(&self, out: &mut [LogRecord]) -> usize {
        let mut cursor = self.log_cursor.lock().unwrap();
        self.block().logs.drain(&mut cursor, out)
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS;
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as *mut _,
            });
            let _ = CloseHandle(self.handle);
        }
    }
}

fn reject_to_err(r: HeaderReject) -> HostError {
    match r {
        HeaderReject::NotReady => HostError::NotConnected,
        HeaderReject::BadMagic => HostError::Handshake("bad magic".into()),
        HeaderReject::MajorMismatch { bridge, host } => HostError::ProtocolMismatch {
            bridge_major: bridge,
            host_major: host,
        },
        HeaderReject::SizeMismatch { bridge, host } => {
            HostError::Handshake(format!("block size mismatch bridge={bridge} host={host}"))
        }
    }
}
