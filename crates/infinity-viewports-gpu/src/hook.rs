//! D3D12 vtable-slot hook installation.
//!
//! We never patch code bytes. To hook a COM method we read the interface's
//! vtable from a throwaway instance we create ourselves, then swap one slot in
//! that vtable; the vtable is shared by every instance of that class in the
//! process, so the sim's own objects call the hook too.
//!
//! # No swap chain, no `Present` hook
//!
//! This module used to create a throwaway DXGI swap chain as well and swap its
//! `Present` slot for a once-a-frame tick. That is gone, and must not come
//! back. `CreateDXGIFactory2` / `CreateSwapChainForHwnd` are intercepted by
//! every graphics injector a user may run (Streamline's `sl.interposer`,
//! ReShade, OptiScaler / OptiShade), so the "throwaway" swap chain was really
//! a second swap chain announced to all of them and then destroyed. Measured
//! on 23 Nov 2026 (MSFS 2024 1.9.24, OptiShade + ReShade 6.8 + Streamline
//! 2.13/2.14): from that moment the injectors' chain handed the real
//! `CDXGISwapChain::Present` a `this` that was OptiScaler's wrapper object,
//! and DXGI read a garbage pointer at `this+0x820` on every present. With DLSS
//! frame generation on, that is an access violation on `sl.dlss_g`'s present
//! thread and the sim dies; with it off, Streamline catches the fault and
//! writes an 8 MB minidump per frame, which is a sim at one frame a second.
//! The slot swap had also landed on `sl.interposer`'s proxy vtable, not
//! DXGI's: the old assumption that a dummy object's vtable is the system
//! module's does not hold once anything wraps the factory.
//!
//! The frame tick now comes from the stream itself ([`crate::stream`]: a slot
//! holding both of its layers is a finished nose frame), and from the service
//! thread while only a layer scan is running ([`crate::capture::idle_tick`]).
//!
//! The throwaway DEVICE and COMMAND LIST remain. They too may come back
//! wrapped (ReShade's proxies, on the machine above); hooking a wrapper's
//! vtable still sees every call the sim makes through it, and the install log
//! names the module each hooked vtable lives in so a report says which.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_RENDER_TARGET_VIEW_DESC,
    D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_DESC, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
    D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12CreateDevice, ID3D12CommandAllocator,
    ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};
use windows::core::GUID;
use windows::core::{Interface, PCWSTR};

/// `ID3D12Device` vtable slots: `CreateCommittedResource` is 27 and
/// `CreatePlacedResource` is 29 (IUnknown 0-2, ID3D12Object 3-6, then the
/// ID3D12Device methods 7.. in declaration order).
const CCR_SLOT: usize = 27;
const CPR_SLOT: usize = 29;
/// `ID3D12GraphicsCommandList` vtable slot for `ResourceBarrier` (IUnknown 0-2,
/// ID3D12Object 3-6, ID3D12DeviceChild 7, ID3D12CommandList 8 (GetType), then
/// the graphics methods 9.. with `ResourceBarrier` at 26).
const RB_SLOT: usize = 26;
/// `ID3D12Device::CreateRenderTargetView` (device methods start at 7;
/// CreateRenderTargetView is 20). Same vtable as `CreateCommittedResource`.
const CRTV_SLOT: usize = 20;
/// `ID3D12GraphicsCommandList::Close` (ID3D12CommandList method after GetType).
const CLOSE_SLOT: usize = 9;
/// `ID3D12GraphicsCommandList::Reset` (Close is 9, Reset is 10).
const RESET_SLOT: usize = 10;
/// `ID3D12GraphicsCommandList::OMSetRenderTargets` (SOSetTargets is 45, this is 46).
const OM_SLOT: usize = 46;

/// Capture candidates noted per `ResourceBarrier` call; the stream wants two
/// layers a frame, so a batch with more readable transitions than this only
/// defers the rest to a later barrier.
const MAX_CAPTURES_PER_CALL: usize = 16;
/// `D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | PIXEL_SHADER_RESOURCE`.
const SRV_STATES: u32 = 0x40 | 0x80;
/// `D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY`: a split barrier's first half —
/// the resource is NOT in `StateAfter` until the matching END_ONLY.
const BARRIER_FLAG_BEGIN_ONLY: u32 = 0x1;
/// `D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES`.
const ALL_SUBRESOURCES: u32 = 0xFFFF_FFFF;

/// `ID3D12GraphicsCommandList::ResourceBarrier` signature.
type ResourceBarrierFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER);
/// `ID3D12Device::CreateRenderTargetView` signature. The destination handle is passed by value.
type CreateRtvFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const D3D12_RENDER_TARGET_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
) -> windows::core::HRESULT;
/// `ID3D12GraphicsCommandList::Close` signature.
type CloseFn = unsafe extern "system" fn(*mut c_void) -> windows::core::HRESULT;
/// `ID3D12GraphicsCommandList::Reset` signature.
type ResetFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> windows::core::HRESULT;
/// `ID3D12GraphicsCommandList::OMSetRenderTargets` signature. The range flag is a Win32 BOOL (i32).
type OmSetFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    i32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
);
/// `ID3D12Device::CreateCommittedResource` signature. Only `p_desc` is read;
/// every argument is forwarded unchanged, so a misread of the desc cannot
/// corrupt the call.
type CreateCommittedFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    i32,
    *const D3D12_RESOURCE_DESC,
    i32,
    *const c_void,
    *const GUID,
    *mut *mut c_void,
) -> windows::core::HRESULT;
/// `ID3D12Device::CreatePlacedResource` signature.
type CreatePlacedFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *const D3D12_RESOURCE_DESC,
    i32,
    *const c_void,
    *const GUID,
    *mut *mut c_void,
) -> windows::core::HRESULT;

static INSTALLED: AtomicBool = AtomicBool::new(false);
// Address of each swapped vtable slot (the `*mut usize` entry), recorded at
// install so teardown can write the original pointer straight back.
static CCR_ENTRY: AtomicUsize = AtomicUsize::new(0);
static CPR_ENTRY: AtomicUsize = AtomicUsize::new(0);
static RB_ENTRY: AtomicUsize = AtomicUsize::new(0);

static CCR_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static CPR_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static RB_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static RB_CALLS: AtomicU64 = AtomicU64::new(0);
static CRTV_ENTRY: AtomicUsize = AtomicUsize::new(0);
static CRTV_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static CLOSE_ENTRY: AtomicUsize = AtomicUsize::new(0);
static CLOSE_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static RESET_ENTRY: AtomicUsize = AtomicUsize::new(0);
static RESET_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static OM_ENTRY: AtomicUsize = AtomicUsize::new(0);
static OM_ORIGINAL: AtomicUsize = AtomicUsize::new(0);

/// Distinct render-target / depth-stencil resource descriptions observed, either
/// at creation or (the persistent ones) via `ResourceBarrier` transitions.
/// Bounded; the hook never blocks a sim thread (it `try_lock`s and skips on
/// contention — a missed record is recovered next time).
static INVENTORY: Mutex<Vec<RtEntry>> = Mutex::new(Vec::new());
/// Resource pointers already `GetDesc`'d from a barrier, so we do it once each.
static SEEN: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// One distinct render-target / depth-stencil resource signature.
#[derive(Clone, Copy, Debug)]
pub struct RtEntry {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub flags: u32,
    pub samples: u32,
    pub dimension: u8,
    /// Seen bound/transitioned during live rendering (not just created).
    pub live: bool,
    pub count: u64,
}

/// Snapshot the render-target inventory.
pub fn rt_inventory() -> Vec<RtEntry> {
    INVENTORY.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Number of `ResourceBarrier` calls observed (0 ⇒ the sim uses enhanced
/// barriers or a different path, and we must hook that instead).
pub fn resource_barrier_calls() -> u64 {
    RB_CALLS.load(Ordering::Relaxed)
}

/// Merge one render-target/depth-stencil signature into the inventory.
fn record(format: u32, width: u32, height: u32, flags: u32, samples: u32, dim: u8, live: bool) {
    if let Ok(mut inv) = INVENTORY.try_lock() {
        if let Some(e) = inv.iter_mut().find(|e| {
            e.format == format
                && e.width == width
                && e.height == height
                && e.flags == flags
                && e.samples == samples
                && e.dimension == dim
        }) {
            e.count += 1;
            e.live |= live;
        } else if inv.len() < 256 {
            inv.push(RtEntry {
                format,
                width,
                height,
                flags,
                samples,
                dimension: dim,
                live,
                count: 1,
            });
        }
    }
}

/// Record a resource description if it is a render target or depth-stencil.
fn record_desc(d: &D3D12_RESOURCE_DESC, live: bool) {
    let rt_ds =
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0;
    if d.Flags.0 & rt_ds == 0 {
        return;
    }
    record(
        d.Format.0 as u32,
        d.Width as u32,
        d.Height,
        d.Flags.0 as u32,
        d.SampleDesc.Count,
        d.Dimension.0 as u8,
        live,
    );
}

/// Record a resource at creation (from a `*const D3D12_RESOURCE_DESC`).
fn record_resource(desc: *const D3D12_RESOURCE_DESC) {
    if desc.is_null() {
        return;
    }
    record_desc(unsafe { &*desc }, false);
}

pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Our replacement `CreateCommittedResource`: record the desc, tail-call.
unsafe extern "system" fn ccr_hook(
    this: *mut c_void,
    hp: *const c_void,
    hf: i32,
    desc: *const D3D12_RESOURCE_DESC,
    st: i32,
    cv: *const c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> windows::core::HRESULT {
    record_resource(desc);
    let orig: CreateCommittedFn =
        unsafe { core::mem::transmute(CCR_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, hp, hf, desc, st, cv, riid, ppv) }
}

/// Our replacement `CreatePlacedResource`: record the desc, tail-call.
unsafe extern "system" fn cpr_hook(
    this: *mut c_void,
    heap: *mut c_void,
    off: u64,
    desc: *const D3D12_RESOURCE_DESC,
    st: i32,
    cv: *const c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> windows::core::HRESULT {
    record_resource(desc);
    let orig: CreatePlacedFn =
        unsafe { core::mem::transmute(CPR_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, heap, off, desc, st, cv, riid, ppv) }
}

/// Whether the command-list hooks have work to do: a stream or a layer scan
/// is running. The bridge stays in the sim for the sim's lifetime (a second
/// install into a live sim once hung it), so its hooks run on every barrier,
/// bind, close and reset of every frame even with the FLIR off. While idle
/// they forward straight to the original: two atomic loads, no lock.
/// `crate::stream::start_stream` clears the cloud latch state before this
/// turns true again, so nothing latched while idle is acted on.
fn active() -> bool {
    crate::stream::stream_active() || crate::capture::scan_pending()
}

/// Our replacement `ResourceBarrier`: for each TRANSITION barrier, read the
/// transitioned resource and `GetDesc` it once, then call the original.
/// This catches the persistent HDR/G-buffer/depth targets the creation hooks
/// miss (they were allocated before injection). Forwards everything unchanged;
/// capture copies are recorded AFTER the original, never before it.
unsafe extern "system" fn rb_hook(
    this: *mut c_void,
    num: u32,
    barriers: *const D3D12_RESOURCE_BARRIER,
) {
    // Barriers recorded for the post-merge copy must not re-enter the copy;
    // with nothing streaming there is nothing to look at.
    if crate::cloud::copying() || !active() {
        let orig: ResourceBarrierFn =
            unsafe { core::mem::transmute(RB_ORIGINAL.load(Ordering::Acquire)) };
        unsafe { orig(this, num, barriers) };
        return;
    }
    // A barrier between the scene colour and the cloud merge means that colour
    // is no longer the bound target. Cleared before the original runs.
    // The post-merge copy stays armed across barriers on other resources.
    crate::cloud::forget(this);
    RB_CALLS.fetch_add(1, Ordering::Relaxed);
    let mut pass_ended: Option<u32> = None;
    let scan_pending = crate::capture::scan_pending();
    let stream_active = crate::stream::stream_active();
    // (resource, subresource, state_after) to copy once the original has run.
    let mut pending = [(0usize, 0u32, 0u32); MAX_CAPTURES_PER_CALL];
    let mut pending_n = 0usize;
    if !barriers.is_null() && num > 0 && num < 4096 {
        for i in 0..num as usize {
            let b = unsafe { barriers.add(i) };
            // Type is the first u32 (0 == D3D12_RESOURCE_BARRIER_TYPE_TRANSITION);
            // for a transition the resource pointer is at byte offset 8.
            let ty = unsafe { *(b as *const u32) };
            if ty != 0 {
                continue;
            }
            let res_ptr = unsafe { *((b as *const u8).add(8) as *const usize) };
            if res_ptr == 0 {
                continue;
            }
            // StateBefore sits at byte 20 of a transition barrier. The cloud
            // colour is still in that state; the copy has to run before this
            // transition. One match per call is enough.
            if pass_ended.is_none() && crate::cloud::barrier_ends_pass(this, res_ptr) {
                pass_ended = Some(unsafe { *((b as *const u8).add(20) as *const u32) });
            }
            // GetDesc once per distinct resource pointer.
            let first_seen = match SEEN.try_lock() {
                Ok(mut g) => {
                    let set = g.get_or_insert_with(HashSet::new);
                    if set.len() < 16384 {
                        set.insert(res_ptr)
                    } else {
                        false
                    }
                }
                Err(_) => false,
            };
            if first_seen {
                let raw = res_ptr as *mut c_void;
                if let Some(res) = unsafe { ID3D12Resource::from_raw_borrowed(&raw) } {
                    let d = unsafe { res.GetDesc() };
                    record_desc(&d, true);
                }
            }

            // Capture candidates: resources this call COMPLETES a transition
            // of into a shader-readable state. Only noted here — the copy is
            // recorded after the original barrier (below), see `capture_ok`.
            if (scan_pending || stream_active) && pending_n < MAX_CAPTURES_PER_CALL {
                let flags = unsafe { *((b as *const u8).add(4) as *const u32) };
                let subresource = unsafe { *((b as *const u8).add(16) as *const u32) };
                let state_after = unsafe { *((b as *const u8).add(24) as *const u32) };
                let readable = state_after & SRV_STATES != 0;
                if readable
                    && flags & BARRIER_FLAG_BEGIN_ONLY == 0
                    && let Some(slot) = pending.get_mut(pending_n)
                {
                    *slot = (res_ptr, subresource, state_after);
                    pending_n += 1;
                }
            }
        }
    }
    if let Some(state_before) = pass_ended {
        unsafe { crate::cloud::flush_pending(this, state_before) };
    }
    let orig: ResourceBarrierFn =
        unsafe { core::mem::transmute(RB_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, num, barriers) };

    // Record the copies AFTER the sim's barrier, so each resource really is in
    // `state_after` when our `state_after -> COPY_SOURCE` transition runs.
    // Recording them before it (as this hook once did) told the driver the
    // resource was already shader-readable while it was still a render target:
    // no decompress / cache flush before the copy. NVIDIA hung the device
    // (DXGI_ERROR_DEVICE_HUNG) and AMD copied compressed garbage (the FLIR read
    // white) the moment the stream started.
    for &(res_ptr, subresource, state_after) in pending.iter().take(pending_n) {
        let raw = res_ptr as *mut c_void;
        let Some(res) = (unsafe { ID3D12Resource::from_raw_borrowed(&raw) }) else {
            continue;
        };
        let d = unsafe { res.GetDesc() };
        if stream_active {
            crate::stream::note_candidate(&d);
        }
        if !capture_ok(&d, subresource) {
            continue;
        }
        if scan_pending {
            unsafe { crate::capture::record_scan_layer(this, res, &d, state_after) };
        }
        if stream_active {
            unsafe { crate::stream::record_stream_layer(this, res, &d, state_after) };
        }
    }
}

/// The copy helpers transition `ALL_SUBRESOURCES` and copy subresource 0, so a
/// barrier is only safe to piggy-back on when it moved the whole resource: a
/// barrier naming every subresource, or subresource 0 of a texture that has
/// no other (one mip, one slice, and not a depth-stencil, whose stencil is a
/// second plane).
fn capture_ok(d: &D3D12_RESOURCE_DESC, subresource: u32) -> bool {
    if subresource == ALL_SUBRESOURCES {
        return true;
    }
    let depth_stencil = d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0;
    subresource == 0 && d.MipLevels <= 1 && d.DepthOrArraySize <= 1 && !depth_stencil
}

/// Our replacement `CreateRenderTargetView`: create the view, then remember it
/// when it is a nose-sized cloud-merge target. The original is the saved
/// function pointer, so a redirect that creates a view does not recurse.
unsafe extern "system" fn crtv_hook(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const D3D12_RENDER_TARGET_VIEW_DESC,
    dest: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> windows::core::HRESULT {
    let orig: CreateRtvFn = unsafe { core::mem::transmute(CRTV_ORIGINAL.load(Ordering::Acquire)) };
    let hr = unsafe { orig(this, resource, desc, dest) };
    if hr.is_ok() && dest.ptr != 0 {
        crate::cloud::note_created(this, resource, desc, dest.ptr);
    }
    hr
}

/// Our replacement `Close`: the cloud pass has finished drawing. Copy the
/// redirected colour while the list is still recording, then close.
unsafe extern "system" fn close_hook(this: *mut c_void) -> windows::core::HRESULT {
    if active() {
        unsafe { crate::cloud::flush_target(this) };
    }
    let orig: CloseFn = unsafe { core::mem::transmute(CLOSE_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this) }
}

/// Our replacement `Reset`: a reused command list must not keep the previous
/// recording's scene colour. Reset discards recorded commands, so a pending
/// copy would be thrown away with them.
unsafe extern "system" fn reset_hook(
    this: *mut c_void,
    alloc: *mut c_void,
    pso: *mut c_void,
) -> windows::core::HRESULT {
    if active() {
        crate::cloud::drop_pending(this);
        crate::cloud::forget(this);
    }
    let orig: ResetFn = unsafe { core::mem::transmute(RESET_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, alloc, pso) }
}

/// Our replacement `OMSetRenderTargets`. Forwards, then maybe rebinds the nose
/// cloud merge onto the nose scene colour.
unsafe extern "system" fn om_hook(
    this: *mut c_void,
    count: u32,
    handles: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    single: i32,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let orig: OmSetFn = unsafe { core::mem::transmute(OM_ORIGINAL.load(Ordering::Acquire)) };
    if !active() {
        unsafe { orig(this, count, handles, single, depth) };
        return;
    }
    unsafe { crate::cloud::on_om(this, count, handles, single, depth, orig) };
}

/// Swap one vtable slot of a COM object to `hook_addr`, returning the original
/// slot value. The object's vtable is shared process-wide, so this installs the
/// hook for every instance of that interface.
unsafe fn swap_slot(
    obj_raw: *mut c_void,
    slot: usize,
    hook_addr: usize,
) -> Result<(usize, usize), String> {
    unsafe {
        let vtable = *(obj_raw as *const *mut usize);
        let entry = vtable.add(slot);
        let original = *entry;
        let mut old = PAGE_PROTECTION_FLAGS::default();
        VirtualProtect(
            entry as *const c_void,
            size_of::<usize>(),
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
        .map_err(|e| format!("VirtualProtect(rw): {e}"))?;
        *entry = hook_addr;
        let mut restored = PAGE_PROTECTION_FLAGS::default();
        let _ = VirtualProtect(
            entry as *const c_void,
            size_of::<usize>(),
            old,
            &mut restored,
        );
        Ok((entry as usize, original))
    }
}

/// Write `original` back into a swapped slot, but only if it still holds our
/// hook (so a foreign hook chained after us is never clobbered). Returns whether
/// the slot was restored.
unsafe fn restore_slot(entry: usize, hook_addr: usize, original: usize) -> bool {
    if entry == 0 || original == 0 {
        return false;
    }
    unsafe {
        let p = entry as *mut usize;
        let mut old = PAGE_PROTECTION_FLAGS::default();
        if VirtualProtect(
            p as *const c_void,
            size_of::<usize>(),
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
        .is_err()
        {
            return false;
        }
        let atomic = AtomicUsize::from_ptr(p);
        let ok = atomic
            .compare_exchange(hook_addr, original, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        let mut restored = PAGE_PROTECTION_FLAGS::default();
        let _ = VirtualProtect(p as *const c_void, size_of::<usize>(), old, &mut restored);
        ok
    }
}

/// Restore every hooked vtable slot to the pointer the sim shipped and mark the
/// hooks uninstalled. Idempotent. The shared process-wide vtables are left
/// exactly as found, so the render path is intact and the hooks can be
/// reinstalled later (a fresh `install_present`). Callers must drain in-flight
/// hook calls (a few presents) before unloading the module.
/// Write every swapped slot back, then forget the bookkeeping so a later
/// install records the pointer it actually finds. Slots that were never swapped
/// (entry is 0) contribute nothing.
unsafe fn restore_installed_slots() -> u32 {
    unsafe {
        restore_one(
            &CCR_ENTRY,
            &CCR_ORIGINAL,
            ccr_hook as CreateCommittedFn as usize,
        ) + restore_one(
            &CPR_ENTRY,
            &CPR_ORIGINAL,
            cpr_hook as CreatePlacedFn as usize,
        ) + restore_one(
            &RB_ENTRY,
            &RB_ORIGINAL,
            rb_hook as ResourceBarrierFn as usize,
        ) + restore_one(
            &CRTV_ENTRY,
            &CRTV_ORIGINAL,
            crtv_hook as CreateRtvFn as usize,
        ) + restore_one(
            &CLOSE_ENTRY,
            &CLOSE_ORIGINAL,
            close_hook as CloseFn as usize,
        ) + restore_one(
            &RESET_ENTRY,
            &RESET_ORIGINAL,
            reset_hook as ResetFn as usize,
        ) + restore_one(&OM_ENTRY, &OM_ORIGINAL, om_hook as OmSetFn as usize)
    }
}

unsafe fn restore_one(entry: &AtomicUsize, original: &AtomicUsize, hook: usize) -> u32 {
    let ok = unsafe {
        restore_slot(
            entry.load(Ordering::Acquire),
            hook,
            original.load(Ordering::Acquire),
        )
    };
    // Drop the slot address so a second restore is a no-op. Keep the original
    // function pointer: a hook call already in flight still tail-calls it.
    entry.store(0, Ordering::Release);
    u32::from(ok)
}

pub fn uninstall_hooks() -> String {
    if !INSTALLED.swap(false, Ordering::AcqRel) {
        return "hooks were not installed".into();
    }
    let n = unsafe { restore_installed_slots() };
    format!("restored {n}/{HOOKED_SLOTS} hooked vtable slots; render path left as found")
}

/// Vtable slots [`install_hooks`] swaps.
const HOOKED_SLOTS: u32 = 7;

/// Install the D3D12 vtable hooks. Idempotent. Returns a description on
/// success, or a reason string on failure.
///
/// `step` is called with a short label BEFORE each OS/D3D call that could
/// block, so a log written from it names the call a hung install is stuck in
/// (a second install into a live sim once never returned, and the only line
/// logged was the one before it).
pub fn install_hooks(step: &mut dyn FnMut(&str)) -> Result<String, String> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Ok("hooks already installed".into());
    }
    // On any failure, allow a later retry.
    match unsafe { install_inner(step) } {
        Ok(s) => Ok(s),
        Err(e) => {
            // A partial swap must be undone. Retrying on top of our own hook
            // would record that hook as the original and recurse.
            let n = unsafe { restore_installed_slots() };
            INSTALLED.store(false, Ordering::Release);
            Err(format!("{e} (rolled back {n} slots)"))
        }
    }
}

/// File name of the module `addr` lies in, for the install log: whose vtable
/// a hook really landed on (`D3D12Core.dll`, or a wrapper's such as
/// `ReShade64.dll`). Never fails; an address in no module says so.
fn module_of(addr: usize) -> String {
    let mut module = HMODULE::default();
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut module,
        )
    };
    if found.is_err() || module.is_invalid() {
        return "no module".into();
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let path = String::from_utf16_lossy(buf.get(..n).unwrap_or(&[]));
    path.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("unnamed module")
        .to_string()
}

unsafe fn install_inner(step: &mut dyn FnMut(&str)) -> Result<String, String> {
    unsafe {
        // A throwaway device and one command list off it: the two vtables the
        // hooks live on. NO command queue and NO swap chain (module header):
        // both are things Streamline's frame generation tracks.
        step("D3D12CreateDevice");
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device)
            .map_err(|e| format!("D3D12CreateDevice: {e}"))?;
        let device = device.ok_or("D3D12CreateDevice returned no device")?;

        step("CreateCommandList");
        let alloc: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map_err(|e| format!("CreateCommandAllocator: {e}"))?;
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
            .map_err(|e| format!("CreateCommandList: {e}"))?;
        let _ = list.Close();

        let dev_raw = device.as_raw();
        let list_raw = list.as_raw();
        let dev_vtbl = *(dev_raw as *const usize);
        let list_vtbl = *(list_raw as *const usize);
        let owners = format!(
            "device vtable {dev_vtbl:#x} in {}, command-list vtable {list_vtbl:#x} in {}",
            module_of(dev_vtbl),
            module_of(list_vtbl)
        );

        // ID3D12Device: the two resource-creation slots and the cloud
        // redirect's CreateRenderTargetView (one shared device vtable).
        step("swap device slots");
        let (ccr_entry, ccr_orig) =
            swap_slot(dev_raw, CCR_SLOT, ccr_hook as CreateCommittedFn as usize)?;
        CCR_ENTRY.store(ccr_entry, Ordering::Release);
        CCR_ORIGINAL.store(ccr_orig, Ordering::Release);
        let (cpr_entry, cpr_orig) =
            swap_slot(dev_raw, CPR_SLOT, cpr_hook as CreatePlacedFn as usize)?;
        CPR_ENTRY.store(cpr_entry, Ordering::Release);
        CPR_ORIGINAL.store(cpr_orig, Ordering::Release);
        let (crtv_entry, crtv_orig) =
            swap_slot(dev_raw, CRTV_SLOT, crtv_hook as CreateRtvFn as usize)?;
        CRTV_ENTRY.store(crtv_entry, Ordering::Release);
        CRTV_ORIGINAL.store(crtv_orig, Ordering::Release);

        // ID3D12GraphicsCommandList: ResourceBarrier (the capture point) and
        // the cloud redirect's Close / Reset / OMSetRenderTargets.
        step("swap command-list slots");
        let (rb_entry, rb_orig) =
            swap_slot(list_raw, RB_SLOT, rb_hook as ResourceBarrierFn as usize)?;
        RB_ENTRY.store(rb_entry, Ordering::Release);
        RB_ORIGINAL.store(rb_orig, Ordering::Release);
        let (close_entry, close_orig) =
            swap_slot(list_raw, CLOSE_SLOT, close_hook as CloseFn as usize)?;
        CLOSE_ENTRY.store(close_entry, Ordering::Release);
        CLOSE_ORIGINAL.store(close_orig, Ordering::Release);
        let (reset_entry, reset_orig) =
            swap_slot(list_raw, RESET_SLOT, reset_hook as ResetFn as usize)?;
        RESET_ENTRY.store(reset_entry, Ordering::Release);
        RESET_ORIGINAL.store(reset_orig, Ordering::Release);
        let (om_entry, om_orig) = swap_slot(list_raw, OM_SLOT, om_hook as OmSetFn as usize)?;
        OM_ENTRY.store(om_entry, Ordering::Release);
        OM_ORIGINAL.store(om_orig, Ordering::Release);

        Ok(format!(
            "hooks installed ({owners}): CreateCommittedResource (slot {CCR_SLOT} orig {ccr_orig:#x}); CreatePlacedResource (slot {CPR_SLOT} orig {cpr_orig:#x}); CreateRenderTargetView (slot {CRTV_SLOT} orig {crtv_orig:#x}); ResourceBarrier (slot {RB_SLOT} orig {rb_orig:#x}); Close (slot {CLOSE_SLOT} orig {close_orig:#x}); Reset (slot {RESET_SLOT} orig {reset_orig:#x}); OMSetRenderTargets (slot {OM_SLOT} orig {om_orig:#x})"
        ))
        // list/alloc/device drop here. The patched vtables persist.
    }
}
