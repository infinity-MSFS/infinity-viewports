use std::path::{Path, PathBuf};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPPROCESS,
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

/// The full path of the module named `file_name` (e.g.
/// `infinity_viewports_bridge.dll`, case-insensitive) loaded in `pid`, or
/// `None` when it is not loaded. This is the truth about what is injected:
/// a named mapping can outlive the DLL that created it, so a channel that
/// opens does not prove the bridge is still there.
pub fn loaded_module(pid: u32, file_name: &str) -> Result<Option<PathBuf>> {
    unsafe {
        // ERROR_BAD_LENGTH while the process is starting: retry briefly.
        let mut snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
        for _ in 0..5 {
            if snap.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
        }
        let snap = snap.map_err(|e| HostError::Os(format!("module snapshot of {pid}: {e}")))?;
        let mut entry = MODULEENTRY32W {
            dwSize: size_of::<MODULEENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Module32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name = wide_str(&entry.szModule);
                if name.eq_ignore_ascii_case(file_name) {
                    found = Some(PathBuf::from(wide_str(&entry.szExePath)));
                    break;
                }
                if Module32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        Ok(found)
    }
}

fn wide_str(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_module_loaded_in_a_process() {
        let me = std::process::id();
        let k32 = loaded_module(me, "KERNEL32.dll").unwrap().unwrap();
        assert!(
            k32.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("kernel32.dll")
        );
        assert_eq!(
            loaded_module(me, "infinity_viewports_nothing.dll").unwrap(),
            None
        );
    }
}
