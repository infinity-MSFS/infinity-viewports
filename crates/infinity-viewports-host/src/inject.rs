use std::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    WaitForSingleObject,
};
use windows::core::w;

use crate::error::{HostError, Result};

pub struct Injected {
    pub pid: u32,
}

pub fn verify_bridge(path: &Path, expected_sha256: Option<&str>) -> Result<()> {
    let bytes = std::fs::read(path).map_err(|e| HostError::BridgeImage(e.to_string()))?;
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return Err(HostError::BridgeImage("not a PE image".into()));
    }
    if let Some(expected) = expected_sha256 {
        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(expected) {
            return Err(HostError::BridgeImage(format!(
                "hash mismatch: got {got}, expected {expected}"
            )));
        }
    }
    Ok(())
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub fn inject(pid: u32, bridge: &Path) -> Result<Injected> {
    let abs = std::fs::canonicalize(bridge)
        .map_err(|e| HostError::Inject(format!("resolve bridge path: {e}")))?;
    let wpath: Vec<u16> = abs.as_os_str().encode_wide().chain(once(0)).collect();
    let nbytes = wpath.len() * 2;

    unsafe {
        let access = PROCESS_CREATE_THREAD
            | PROCESS_VM_OPERATION
            | PROCESS_VM_WRITE
            | PROCESS_VM_READ
            | PROCESS_QUERY_INFORMATION;
        let proc = OpenProcess(access, false, pid)
            .map_err(|e| HostError::Inject(format!("OpenProcess({pid}): {e}")))?;
        let proc = Handle(proc);

        let remote = VirtualAllocEx(
            proc.0,
            None,
            nbytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(HostError::Inject("VirtualAllocEx failed".into()));
        }
        let free_remote = || {
            let _ = VirtualFreeEx(proc.0, remote, 0, MEM_RELEASE);
        };

        let mut written = 0usize;
        if let Err(e) = WriteProcessMemory(
            proc.0,
            remote,
            wpath.as_ptr() as *const c_void,
            nbytes,
            Some(&mut written),
        ) {
            free_remote();
            return Err(HostError::Inject(format!("WriteProcessMemory: {e}")));
        }

        let k32 = GetModuleHandleW(w!("kernel32.dll"))
            .map_err(|e| HostError::Inject(format!("GetModuleHandleW(kernel32): {e}")))?;
        let Some(load_library) = GetProcAddress(k32, windows::core::s!("LoadLibraryW")) else {
            free_remote();
            return Err(HostError::Inject(
                "GetProcAddress(LoadLibraryW) failed".into(),
            ));
        };
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(load_library);

        let thread = match CreateRemoteThread(proc.0, None, 0, Some(start), Some(remote), 0, None) {
            Ok(t) => Handle(t),
            Err(e) => {
                free_remote();
                return Err(HostError::Inject(format!("CreateRemoteThread: {e}")));
            }
        };

        const WAIT_OBJECT_0: u32 = 0;
        let wait = WaitForSingleObject(thread.0, 15_000);
        if wait.0 != WAIT_OBJECT_0 {
            free_remote();
            return Err(HostError::Inject(
                "LoadLibraryW remote thread did not complete in time".into(),
            ));
        }

        let mut code = 0u32;
        let _ = GetExitCodeThread(thread.0, &mut code);
        free_remote();
        if code == 0 {
            return Err(HostError::Inject(
                "LoadLibraryW returned null in target (bridge failed to load)".into(),
            ));
        }
    }
    Ok(Injected { pid })
}

// --- Minimal SHA-256 , so the crate carries no hashing dep ---

fn sha256_hex(data: &[u8]) -> String {
    let h = sha256(data);
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, wi) in w.iter_mut().take(16).enumerate() {
            *wi = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
