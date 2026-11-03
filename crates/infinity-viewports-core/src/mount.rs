use std::ffi::c_void;

use infinity_viewports_protocol::{
    APPLIED_FOLLOW, APPLIED_MISSING, APPLIED_PARENTED, APPLIED_PLACED,
};
use infinity_viewports_shim::{guarded_attach, guarded_detach, guarded_read};

use crate::contract::{
    ATTACH_CHILD, DETACH_NODE, NODE_FIRST_CHILD, NODE_NEXT_SIBLING, NODE_PARENT, NODE_POSITION,
    NODE_WORLD_MATRIX, VIEW_NODE_HANDLE,
};
use crate::pose::{self, Basis};

type Attach = extern "C" fn(*mut c_void, *const c_void, i32);
type Detach = extern "C" fn(*mut c_void, i32, i32);

#[repr(C)]
struct NodeHandle {
    control: u64,
    generation: u32,
    reserved: u32,
}

pub struct Mount {
    base: usize,
    scanned: bool,
    attach: usize,
    detach: usize,
    matrix_ok: Option<bool>,
    root: u64,
    aircraft: u64,
    parented: bool,
    refused: bool,
}

pub struct Mounted {
    pub applied: u32,
    pub parented: u32,
    pub note: &'static str,
}

impl Mount {
    pub fn new(base: usize) -> Mount {
        Mount {
            base,
            scanned: false,
            attach: 0,
            detach: 0,
            matrix_ok: None,
            root: 0,
            aircraft: 0,
            parented: false,
            refused: false,
        }
    }

    pub fn resolve(
        &mut self,
        aircraft: bool,
        pilot_node: u64,
        our_view: u64,
        our_node: u64,
    ) -> Mounted {
        if !aircraft {
            if self.parented {
                self.give_back(our_view, our_node);
            }
            return Mounted {
                applied: if self.parented {
                    APPLIED_PARENTED
                } else {
                    APPLIED_FOLLOW
                },
                parented: u32::from(self.parented),
                note: if self.parented {
                    "still parented; release failed, pose left where it is"
                } else {
                    "following the pilot view"
                },
            };
        }
        let Some(found) = self.airframe(pilot_node) else {
            if self.parented {
                self.give_back(our_view, our_node);
            }
            return Mounted {
                applied: if self.parented {
                    APPLIED_PARENTED
                } else {
                    APPLIED_MISSING
                },
                parented: u32::from(self.parented),
                note: if self.parented {
                    "still parented; release failed, pose left where it is"
                } else if self.matrix_ok == Some(false) {
                    "aircraft matrix rejected; camera held"
                } else {
                    "no aircraft body on the pilot node; camera held"
                },
            };
        };
        let parented = self.ensure_parent(our_view, our_node, found.node);
        Mounted {
            applied: if parented {
                APPLIED_PARENTED
            } else {
                APPLIED_PLACED
            },
            parented: u32::from(parented),
            note: if parented {
                "on the aircraft node"
            } else if self.refused {
                "on the aircraft axes; node was left on its original parent"
            } else {
                "on the aircraft axes"
            },
        }
    }
    /// The aircraft model's axes as the scene holds them now, without
    /// touching the scene graph.
    pub fn frame_basis(&mut self, pilot_node: u64) -> Option<Basis> {
        self.airframe(pilot_node).map(|f| f.basis)
    }

    pub fn diagnose(&self, pilot_node: u64) -> String {
        if pilot_node == 0 {
            return "mount-diag: pilot_node=0".into();
        }
        let eye = match read_f64x3(pilot_node + NODE_POSITION) {
            Ok(e) => e,
            Err(_) => return format!("mount-diag pilot {pilot_node:#x}: NODE_POSITION unreadable"),
        };
        let pr = (eye[0] * eye[0] + eye[1] * eye[1] + eye[2] * eye[2]).sqrt();
        let mut s = format!(
            "mount-diag pilot {pilot_node:#x} |pos|={:.0}km wmat_ok={} walk:",
            pr / 1000.0,
            read_basis(pilot_node).is_some()
        );
        let mut node = pilot_node;
        for i in 0..8 {
            match read_basis(node) {
                Some(b) => {
                    s.push_str(&format!(
                        " [{i}]a={:.0}e={}",
                        distance(b.origin, eye),
                        ecef(b.origin) as u8
                    ));
                }
                None => s.push_str(&format!(" [{i}]nomat")),
            }
            let parent = read_u64(node + NODE_PARENT).unwrap_or(0);
            if parent < 0x10000 || parent == node {
                s.push_str(" END");
                break;
            }
            node = parent;
        }
        s
    }

    fn airframe(&mut self, pilot_node: u64) -> Option<Found> {
        if self.matrix_ok == Some(false) || pilot_node == 0 {
            return None;
        }
        let eye = read_f64x3(pilot_node + NODE_POSITION).ok()?;
        let own = read_basis(pilot_node)?;
        if distance(own.origin, eye) > 2.0 {
            self.matrix_ok = Some(false);
            return None;
        }
        self.matrix_ok = Some(true);
        let mut best: Option<Found> = None;
        let mut node = pilot_node;
        for _ in 0..8 {
            if let Some(basis) = read_basis(node) {
                let away = distance(basis.origin, eye);
                if ecef(basis.origin) && (0.3..200.0).contains(&away) {
                    let replace = best.as_ref().map(|f| away > f.away).unwrap_or(true);
                    if replace {
                        best = Some(Found { basis, node, away });
                    }
                }
            } else if node != pilot_node {
                break;
            }
            let parent = read_u64(node + NODE_PARENT).unwrap_or(0);
            if parent < 0x10000 || parent == node {
                break;
            }
            node = parent;
        }
        best
    }

    fn ensure_parent(&mut self, our_view: u64, our_node: u64, aircraft: u64) -> bool {
        if self.refused || our_node == 0 || aircraft == 0 {
            return false;
        }
        if self.parented && self.aircraft == aircraft {
            let parent = read_u64(our_node + NODE_PARENT).unwrap_or(0);
            if parent == aircraft {
                return true;
            }
            self.refused = true;
            if parent == 0 || parent == self.root {
                self.parented = false;
                self.aircraft = 0;
            }
            return false;
        }
        self.scan();
        if self.attach == 0 || self.detach == 0 {
            self.refused = true;
            return false;
        }
        let current = read_u64(our_node + NODE_PARENT).unwrap_or(0);
        if current == aircraft {
            self.aircraft = aircraft;
            self.parented = true;
            return true;
        }
        if current < 0x10000
            || read_u64(our_node + NODE_FIRST_CHILD).unwrap_or(1) != 0
            || !listed(current, our_node)
        {
            self.refused = true;
            return false;
        }
        let Some(handle) = read_handle(our_view + VIEW_NODE_HANDLE, our_node) else {
            self.refused = true;
            return false;
        };
        let detach: Detach = fn_at(self.detach);
        let attach: Attach = fn_at(self.attach);
        let node = our_node as *mut c_void;
        let ok = unsafe {
            guarded_detach(detach, node, true, false).is_ok()
                && guarded_attach(
                    attach,
                    aircraft as *mut c_void,
                    &handle as *const NodeHandle as *const c_void,
                    false,
                )
                .is_ok()
        };
        if !ok || read_u64(our_node + NODE_PARENT).unwrap_or(0) != aircraft {
            if self.root == 0 {
                self.root = current;
            }
            let _ = unsafe {
                guarded_attach(
                    attach,
                    self.root as *mut c_void,
                    &handle as *const NodeHandle as *const c_void,
                    false,
                )
            };
            self.refused = true;
            self.parented = false;
            return false;
        }
        self.root = current;
        self.aircraft = aircraft;
        self.parented = true;
        true
    }

    fn give_back(&mut self, our_view: u64, our_node: u64) {
        if !self.parented {
            return;
        }
        if self.root == 0 || self.detach == 0 || self.attach == 0 {
            self.refused = true;
            return;
        }
        let Some(handle) = read_handle(our_view + VIEW_NODE_HANDLE, our_node) else {
            self.refused = true;
            return;
        };
        let detach: Detach = fn_at(self.detach);
        let attach: Attach = fn_at(self.attach);
        let node = our_node as *mut c_void;
        let ok = unsafe {
            guarded_detach(detach, node, true, false).is_ok()
                && guarded_attach(
                    attach,
                    self.root as *mut c_void,
                    &handle as *const NodeHandle as *const c_void,
                    false,
                )
                .is_ok()
        };
        if ok && read_u64(our_node + NODE_PARENT).unwrap_or(0) == self.root {
            self.parented = false;
            self.aircraft = 0;
        } else {
            self.refused = true;
        }
    }

    fn scan(&mut self) {
        if self.scanned {
            return;
        }
        self.scanned = true;
        self.attach = bind_fn(self.base, ATTACH_CHILD);
        self.detach = bind_fn(self.base, DETACH_NODE);
    }
}

fn bind_fn(base: usize, rva: u32) -> usize {
    let addr = base + rva as usize;
    let mut bytes = [0u8; 8];
    if unsafe { guarded_read(&mut bytes, addr as *const u8) }.is_err() {
        return 0;
    }
    if looks_callable(&bytes) { addr } else { 0 }
}

fn looks_callable(bytes: &[u8; 8]) -> bool {
    if bytes.iter().all(|&b| b == 0 || b == 0xCC) {
        return false;
    }
    matches!(
        bytes[0],
        0x40 | 0x41 | 0x48 | 0x4C | 0x4D | 0x53 | 0x55 | 0x56 | 0x57 | 0x0F
    )
}

struct Found {
    basis: Basis,
    node: u64,
    away: f64,
}

fn read_basis(node: u64) -> Option<Basis> {
    let matrix = read_u64(node + NODE_WORLD_MATRIX).ok()?;
    if matrix < 0x10000 {
        return None;
    }
    let mut rows = [[0f64; 3]; 4];
    for row in 0..4 {
        let mut buf = [0u8; 24];
        unsafe { guarded_read(&mut buf, (matrix + row as u64 * 32) as *const u8) }.ok()?;
        for k in 0..3 {
            rows[row][k] = f64::from_le_bytes(buf[k * 8..k * 8 + 8].try_into().ok()?);
        }
    }
    pose::basis_from_rows(rows)
}

fn read_handle(addr: u64, expect_node: u64) -> Option<NodeHandle> {
    let mut buf = [0u8; 12];
    unsafe { guarded_read(&mut buf, addr as *const u8) }.ok()?;
    let control = u64::from_le_bytes(buf[0..8].try_into().ok()?);
    let generation = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    if control < 0x10000 {
        return None;
    }
    let control_generation = read_u32(control + 28).ok()?;
    let payload = read_u64(control).ok()?;
    if control_generation != generation || payload != expect_node {
        return None;
    }
    Some(NodeHandle {
        control,
        generation,
        reserved: 0,
    })
}

fn listed(parent: u64, node: u64) -> bool {
    let mut child = match read_u64(parent + NODE_FIRST_CHILD) {
        Ok(c) => c,
        Err(_) => return false,
    };
    for _ in 0..256 {
        if child == 0 {
            return false;
        }
        if child == node {
            return true;
        }
        child = match read_u64(child + NODE_NEXT_SIBLING) {
            Ok(c) => c,
            Err(_) => return false,
        };
    }
    false
}

fn ecef(p: [f64; 3]) -> bool {
    let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
    (6_300_000.0..6_500_000.0).contains(&r)
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn read_f64x3(addr: u64) -> Result<[f64; 3], ()> {
    Ok([read_f64(addr)?, read_f64(addr + 8)?, read_f64(addr + 16)?])
}

fn read_f64(addr: u64) -> Result<f64, ()> {
    Ok(f64::from_le_bytes(read_bytes(addr)?))
}

fn read_u32(addr: u64) -> Result<u32, ()> {
    let mut buf = [0u8; 4];
    unsafe { guarded_read(&mut buf, addr as *const u8) }.map_err(|_| ())?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(addr: u64) -> Result<u64, ()> {
    let mut buf = [0u8; 8];
    unsafe { guarded_read(&mut buf, addr as *const u8) }.map_err(|_| ())?;
    Ok(u64::from_le_bytes(buf))
}

fn read_bytes(addr: u64) -> Result<[u8; 8], ()> {
    let mut buf = [0u8; 8];
    unsafe { guarded_read(&mut buf, addr as *const u8) }.map_err(|_| ())?;
    Ok(buf)
}

fn fn_at<T: Copy>(addr: usize) -> T {
    assert_eq!(size_of::<T>(), size_of::<usize>());
    unsafe { std::mem::transmute_copy(&addr) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callable_prologue_rejects_padding() {
        assert!(looks_callable(&[
            0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83
        ]));
        assert!(looks_callable(&[
            0x0f, 0xb6, 0x00, 0xc3, 0x90, 0x90, 0x90, 0x90
        ]));
        assert!(!looks_callable(&[0x00; 8]));
        assert!(!looks_callable(&[0xcc; 8]));
        assert!(!looks_callable(&[
            0xc3, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90
        ]));
    }
}
