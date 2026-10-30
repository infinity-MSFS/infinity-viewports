use std::path::{Path, PathBuf};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

use crate::error::{HostError, Result};

#[derive(Debug, Clone)]
pub struct SimProcess {
    pub pid: u32,
    pub exe_path: Option<PathBuf>,
}

const EXE_NAME: &str = "FlightSimulator2024.exe";

pub fn find_all() -> Result<Vec<SimProcess>> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| HostError::Os(e.to_string()))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile);
                let name = name.trim_end_matches('\0');
                if name.eq_ignore_ascii_case(EXE_NAME) {
                    out.push(SimProcess {
                        pid: entry.th32ProcessID,
                        exe_path: None,
                    });
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

pub fn select(configured: Option<&Path>) -> Result<SimProcess> {
    let mut all = find_all()?;
    if all.is_empty() {
        return Err(HostError::SimNotFound);
    }
    all.sort_by_key(|p| p.pid);
    if let Some(cfg) = configured {
        if let Some(p) = all.iter().find(|p| p.exe_path.as_deref() == Some(cfg)) {
            return Ok(p.clone());
        }
    }
    Ok(all.into_iter().next().unwrap())
}
