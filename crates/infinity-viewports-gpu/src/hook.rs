//! D3D12 / DXGI vtable-slot hook installation (Milestone B: `Present`).
//!
//! We never patch code bytes. To hook a COM method we read the interface's
//! vtable from a throwaway instance we create ourselves, then swap one slot in
//! that vtable. DXGI returns a single underlying swap-chain implementation whose
//! vtable is shared by every swap chain in the process, so swapping the
//! `IDXGISwapChain::Present` slot (index 8) on our dummy swap chain's vtable
//! installs the hook for the sim's real swap chain too. After the swap we drop
//! the dummy device/queue/swap chain and destroy the window — the vtable lives
//! in the DXGI module and persists.
//!
//! The hook runs on the sim's present thread, so it does no allocation or
//! logging; it records what it observes (present count, backbuffer dimensions
//! and format) into atomics that the bridge's service thread reads.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_RENDER_TARGET_VIEW_DESC, D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12CreateDevice, ID3D12CommandAllocator, ID3D12CommandQueue, ID3D12Device,
    ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGISwapChain,
    IDXGISwapChain1,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::GUID;
use windows::core::{Interface, w};

/// `IDXGISwapChain::Present` signature.
type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> windows::core::HRESULT;
/// `ID3D12CommandQueue::ExecuteCommandLists` signature.
type EclFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);

/// `IDXGISwapChain` vtable slot for `Present` (IUnknown 0-2, IDXGIObject 3-6,
/// IDXGIDeviceSubObject 7, then `Present` at 8).
const PRESENT_SLOT: usize = 8;
/// `ID3D12CommandQueue` vtable slot for `ExecuteCommandLists` (IUnknown 0-2,
/// ID3D12Object 3-6, ID3D12DeviceChild 7, ID3D12Pageable adds none, then
/// UpdateTileMappings 8, CopyTileMappings 9, `ExecuteCommandLists` 10).
const ECL_SLOT: usize = 10;
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
static PRESENT_ENTRY: AtomicUsize = AtomicUsize::new(0);
static ECL_ENTRY: AtomicUsize = AtomicUsize::new(0);
static CCR_ENTRY: AtomicUsize = AtomicUsize::new(0);
static CPR_ENTRY: AtomicUsize = AtomicUsize::new(0);
static RB_ENTRY: AtomicUsize = AtomicUsize::new(0);
static PRESENT_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);
static BB_W: AtomicU32 = AtomicU32::new(0);
static BB_H: AtomicU32 = AtomicU32::new(0);
static BB_FMT: AtomicU32 = AtomicU32::new(0);

static ECL_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static ECL_SUBMITS: AtomicU64 = AtomicU64::new(0);
static ECL_LISTS: AtomicU64 = AtomicU64::new(0);

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

/// What the hooks have observed so far.
#[derive(Clone, Copy, Debug, Default)]
pub struct Observed {
    pub present_count: u64,
    pub backbuffer_w: u32,
    pub backbuffer_h: u32,
    pub backbuffer_format: u32,
    /// `ExecuteCommandLists` calls observed (all queues).
    pub ecl_submits: u64,
    /// Cumulative command lists submitted.
    pub ecl_lists: u64,
}

/// Snapshot the hooks' observations (lock-free).
pub fn observed() -> Observed {
    Observed {
        present_count: PRESENT_COUNT.load(Ordering::Relaxed),
        backbuffer_w: BB_W.load(Ordering::Relaxed),
        backbuffer_h: BB_H.load(Ordering::Relaxed),
        backbuffer_format: BB_FMT.load(Ordering::Relaxed),
        ecl_submits: ECL_SUBMITS.load(Ordering::Relaxed),
        ecl_lists: ECL_LISTS.load(Ordering::Relaxed),
    }
}

pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Our replacement `Present`: record, then tail-call the original. Runs on the
/// sim's present thread — no allocation, no logging, just atomics.
unsafe extern "system" fn present_hook(
    this: *mut c_void,
    sync_interval: u32,
    flags: u32,
) -> windows::core::HRESULT {
    PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);
    // Capture the backbuffer description once.
    if BB_W.load(Ordering::Relaxed) == 0 {
        if let Some(sc) = unsafe { IDXGISwapChain::from_raw_borrowed(&this) } {
            if let Ok(desc) = unsafe { sc.GetDesc() } {
                BB_H.store(desc.BufferDesc.Height, Ordering::Relaxed);
                BB_FMT.store(desc.BufferDesc.Format.0 as u32, Ordering::Relaxed);
                // Store width last: it is the "ready" flag other reads key on.
                BB_W.store(desc.BufferDesc.Width, Ordering::Release);
            }
        }
    }
    let orig: PresentFn = unsafe { core::mem::transmute(PRESENT_ORIGINAL.load(Ordering::Acquire)) };
    let r = unsafe { orig(this, sync_interval, flags) };
    crate::capture::tick();
    r
}

/// Our replacement `ExecuteCommandLists`: count, then tail-call the original.
/// Read-only — never touches the command lists. Fires for every command queue
/// in the process (the vtable is shared).
unsafe extern "system" fn ecl_hook(this: *mut c_void, num_lists: u32, lists: *const *mut c_void) {
    ECL_SUBMITS.fetch_add(1, Ordering::Relaxed);
    ECL_LISTS.fetch_add(num_lists as u64, Ordering::Relaxed);
    let orig: EclFn = unsafe { core::mem::transmute(ECL_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, num_lists, lists) }
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

/// Our replacement `ResourceBarrier`: for each TRANSITION barrier, read the
/// transitioned resource and `GetDesc` it once, then tail-call the original.
/// This catches the persistent HDR/G-buffer/depth targets the creation hooks
/// miss (they were allocated before injection). Read-only; forwards everything.
unsafe extern "system" fn rb_hook(
    this: *mut c_void,
    num: u32,
    barriers: *const D3D12_RESOURCE_BARRIER,
) {
    // Barriers recorded for the post-merge copy must not re-enter the copy.
    if crate::cloud::copying() {
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

            // Capture: when a layer scan or the live stream is active, look for
            // resources transitioning to a shader-readable state and record a
            // copy into this (the sim's) command list. One GetDesc serves both.
            let scan_pending = crate::capture::scan_pending();
            let stream_active = crate::stream::stream_active();
            if scan_pending || stream_active {
                let state_after = unsafe { *((b as *const u8).add(24) as *const u32) };
                let readable = state_after & 0xC0 != 0; // PIXEL(0x80)|NON_PIXEL(0x40) shader resource
                if readable {
                    let raw = res_ptr as *mut c_void;
                    if let Some(res) = unsafe { ID3D12Resource::from_raw_borrowed(&raw) } {
                        let d = unsafe { res.GetDesc() };
                        if scan_pending {
                            unsafe {
                                crate::capture::record_scan_layer(this, res, &d, state_after)
                            };
                        }
                        if stream_active {
                            unsafe {
                                crate::stream::record_stream_layer(this, res, &d, state_after)
                            };
                        }
                    }
                }
            }
        }
    }
    if let Some(state_before) = pass_ended {
        unsafe { crate::cloud::flush_pending(this, state_before) };
    }
    let orig: ResourceBarrierFn =
        unsafe { core::mem::transmute(RB_ORIGINAL.load(Ordering::Acquire)) };
    unsafe { orig(this, num, barriers) }
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
    unsafe { crate::cloud::flush_target(this) };
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
    crate::cloud::drop_pending(this);
    crate::cloud::forget(this);
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
            &PRESENT_ENTRY,
            &PRESENT_ORIGINAL,
            present_hook as PresentFn as usize,
        ) + restore_one(&ECL_ENTRY, &ECL_ORIGINAL, ecl_hook as EclFn as usize)
            + restore_one(
                &CCR_ENTRY,
                &CCR_ORIGINAL,
                ccr_hook as CreateCommittedFn as usize,
            )
            + restore_one(
                &CPR_ENTRY,
                &CPR_ORIGINAL,
                cpr_hook as CreatePlacedFn as usize,
            )
            + restore_one(
                &RB_ENTRY,
                &RB_ORIGINAL,
                rb_hook as ResourceBarrierFn as usize,
            )
            + restore_one(
                &CRTV_ENTRY,
                &CRTV_ORIGINAL,
                crtv_hook as CreateRtvFn as usize,
            )
            + restore_one(
                &CLOSE_ENTRY,
                &CLOSE_ORIGINAL,
                close_hook as CloseFn as usize,
            )
            + restore_one(
                &RESET_ENTRY,
                &RESET_ORIGINAL,
                reset_hook as ResetFn as usize,
            )
            + restore_one(&OM_ENTRY, &OM_ORIGINAL, om_hook as OmSetFn as usize)
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

pub fn uninstall_present() -> String {
    if !INSTALLED.swap(false, Ordering::AcqRel) {
        return "hooks were not installed".into();
    }
    let n = unsafe { restore_installed_slots() };
    format!("restored {n}/9 hooked vtable slots; render path left as found")
}

/// Install the D3D12/DXGI vtable hooks (`Present` + `ExecuteCommandLists`).
/// Idempotent. Returns a description on success, or a reason string on failure.
pub fn install_present() -> Result<String, String> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Ok("hooks already installed".into());
    }
    // On any failure, allow a later retry.
    match unsafe { install_inner() } {
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

unsafe fn install_inner() -> Result<String, String> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW: {e}"))?
            .into();

        // Minimal hidden window to back the dummy swap chain.
        let class = w!("InfinityViewportsDummyWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // harmless if already registered
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("infinity-viewports"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            8,
            8,
            None,
            None,
            Some(hinst),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;
        let _wnd = WindowGuard(hwnd);

        // Dummy device + direct queue.
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device)
            .map_err(|e| format!("D3D12CreateDevice: {e}"))?;
        let device = device.ok_or("D3D12CreateDevice returned no device")?;
        let qdesc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        };
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&qdesc)
            .map_err(|e| format!("CreateCommandQueue: {e}"))?;

        // Dummy flip swap chain on that queue.
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
            .map_err(|e| format!("CreateDXGIFactory2: {e}"))?;
        let scdesc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 8,
            Height: 8,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let swap: IDXGISwapChain1 = factory
            .CreateSwapChainForHwnd(&queue, hwnd, &scdesc, None, None)
            .map_err(|e| format!("CreateSwapChainForHwnd: {e}"))?;

        // Swap IDXGISwapChain::Present (slot 8).
        let present_vtbl = *(swap.as_raw() as *const *mut usize);
        let (present_entry, present_orig) = swap_slot(
            swap.as_raw(),
            PRESENT_SLOT,
            present_hook as PresentFn as usize,
        )?;
        PRESENT_ENTRY.store(present_entry, Ordering::Release);
        PRESENT_ORIGINAL.store(present_orig, Ordering::Release);

        // Swap ID3D12CommandQueue::ExecuteCommandLists (slot 10).
        let (ecl_entry, ecl_orig) =
            swap_slot(queue.as_raw(), ECL_SLOT, ecl_hook as EclFn as usize)?;
        ECL_ENTRY.store(ecl_entry, Ordering::Release);
        ECL_ORIGINAL.store(ecl_orig, Ordering::Release);

        // Swap ID3D12Device resource-creation slots (shared device vtable).
        let dev_raw = device.as_raw();
        let (ccr_entry, ccr_orig) =
            swap_slot(dev_raw, CCR_SLOT, ccr_hook as CreateCommittedFn as usize)?;
        CCR_ENTRY.store(ccr_entry, Ordering::Release);
        CCR_ORIGINAL.store(ccr_orig, Ordering::Release);
        let (cpr_entry, cpr_orig) =
            swap_slot(dev_raw, CPR_SLOT, cpr_hook as CreatePlacedFn as usize)?;
        CPR_ENTRY.store(cpr_entry, Ordering::Release);
        CPR_ORIGINAL.store(cpr_orig, Ordering::Release);

        // Dummy command list, to get the ID3D12GraphicsCommandList vtable, and
        // swap ResourceBarrier (slot 26).
        let alloc: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map_err(|e| format!("CreateCommandAllocator: {e}"))?;
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
            .map_err(|e| format!("CreateCommandList: {e}"))?;
        let _ = list.Close();
        let (rb_entry, rb_orig) = swap_slot(
            list.as_raw(),
            RB_SLOT,
            rb_hook as ResourceBarrierFn as usize,
        )?;
        RB_ENTRY.store(rb_entry, Ordering::Release);
        RB_ORIGINAL.store(rb_orig, Ordering::Release);

        // Nose cloud redirect. These three sit on the same vtables as the slots
        // above, so a sim that already calls ResourceBarrier / CreateCommittedResource
        // calls these too.
        let (crtv_entry, crtv_orig) =
            swap_slot(dev_raw, CRTV_SLOT, crtv_hook as CreateRtvFn as usize)?;
        CRTV_ENTRY.store(crtv_entry, Ordering::Release);
        CRTV_ORIGINAL.store(crtv_orig, Ordering::Release);
        let list_raw = list.as_raw();
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
            "hooks installed: Present (vtable {present_vtbl:p} slot {PRESENT_SLOT} orig {present_orig:#x}); ExecuteCommandLists (slot {ECL_SLOT} orig {ecl_orig:#x}); CreateCommittedResource (slot {CCR_SLOT} orig {ccr_orig:#x}); CreatePlacedResource (slot {CPR_SLOT} orig {cpr_orig:#x}); ResourceBarrier (slot {RB_SLOT} orig {rb_orig:#x}); CreateRenderTargetView (slot {CRTV_SLOT} orig {crtv_orig:#x}); Close (slot {CLOSE_SLOT} orig {close_orig:#x}); Reset (slot {RESET_SLOT} orig {reset_orig:#x}); OMSetRenderTargets (slot {OM_SLOT} orig {om_orig:#x})"
        ))
        // _wnd drops here -> DestroyWindow; swap/queue/device/factory drop too.
        // The patched DXGI/D3D12-module vtables persist.
    }
}

struct WindowGuard(HWND);
impl Drop for WindowGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}
