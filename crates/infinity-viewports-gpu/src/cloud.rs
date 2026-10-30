//! Nose-view cloud composite.
//!
//! Volumetric clouds are composited by a bind of three targets and no depth:
//! R11G11B10 colour, R8_SNORM, R8_UNORM. On a secondary camera that colour
//! target is a same-size texture the view never samples. When that bind
//! immediately follows the nose scene colour on the same command list, target
//! 0 is replaced with the scene colour. The main view's merge already uses its
//! scene colour as target 0, so the same-resource check leaves it alone.
//!
//! The live stream copies a colour target at its first readable transition.
//! That transition is recorded before this pass draws, so the picture misses
//! the composite. After a redirect, the stream stops taking those early copies
//! and one copy of the redirected colour is recorded when the pass ends: the
//! next bind, a barrier on that colour, or `Close`. The draws are already in
//! the command list by then, and the colour is still the render target unless
//! the barrier says otherwise.
//!
//! Counters are the probe. `shaped == 0` means this view never binds that
//! merge. `shaped > 0` and `merges == 0` means the merge is real but does not
//! follow the nose colour (a barrier, or a different previous target).
//! `redirects > 0` and `posted == 0` means the pass never ended on this vtable.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
    D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_RENDER_TARGET_VIEW_DESC, D3D12_RENDER_TARGET_VIEW_DESC_0,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_STATE_RENDER_TARGET,
    D3D12_RTV_DIMENSION_TEXTURE2D, D3D12_RTV_DIMENSION_TEXTURE2DARRAY, D3D12_TEX2D_RTV,
    ID3D12DescriptorHeap, ID3D12Device, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8_SNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R11G11B10_FLOAT,
};
use windows::core::Interface;

/// Nose scene size. The driver asks the engine for this picture.
const NOSE_W: u32 = 1024;
const NOSE_H: u32 = 576;
const FMT_COLOR: u32 = DXGI_FORMAT_R11G11B10_FLOAT.0 as u32;
const FMT_SNORM: u32 = DXGI_FORMAT_R8_SNORM.0 as u32;
const FMT_UNORM: u32 = DXGI_FORMAT_R8_UNORM.0 as u32;

type OmFn = unsafe extern "system" fn(
    *mut std::ffi::c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    i32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
);

/// One render-target view we have seen created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub resource: usize,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub mip: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloudStats {
    pub tracked: u32,
    /// `OMSetRenderTargets` calls observed. Zero means this vtable is not the one the sim records on.
    pub om_calls: u64,
    pub scene_binds: u64,
    /// Three-target nose-sized merge signature, whether or not it followed the scene colour.
    pub shaped: u64,
    pub merges: u64,
    pub redirects: u64,
    pub refusals: u64,
    /// Copies of the redirected colour recorded after the cloud pass.
    pub posted: u64,
    /// Pass ended, but the copy was not recorded (stream off, or the slot was busy).
    pub misses: u64,
}

struct ListState {
    list: usize,
    scene: Option<View>,
    /// Redirected nose colour waiting until the cloud pass finishes drawing.
    pending: Option<View>,
}

struct LocalHeap {
    _heap: ID3D12DescriptorHeap,
    handles: [D3D12_CPU_DESCRIPTOR_HANDLE; 3],
}

static VIEWS: LazyLock<Mutex<HashMap<usize, View>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LISTS: Mutex<Vec<ListState>> = Mutex::new(Vec::new());
static DEVICE: Mutex<Option<ID3D12Device>> = Mutex::new(None);
static RTV_INCR: AtomicUsize = AtomicUsize::new(0);
/// Lists that currently hold a latched nose colour. Barriers skip the map when this is zero.
static LATCHED: AtomicU64 = AtomicU64::new(0);
static OM_CALLS: AtomicU64 = AtomicU64::new(0);
static SCENE_BINDS: AtomicU64 = AtomicU64::new(0);
static SHAPED: AtomicU64 = AtomicU64::new(0);
static MERGES: AtomicU64 = AtomicU64::new(0);
static REDIRECTS: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);
static POSTED: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Set after the first successful redirect. The stream then ignores early colour copies.
static COLOR_OWNED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HEAP: RefCell<Option<LocalHeap>> = const { RefCell::new(None) };
    /// Inside the post-pass copy. Barriers recorded for that copy must not re-enter it.
    static COPYING: Cell<bool> = const { Cell::new(false) };
}

pub fn stats() -> CloudStats {
    let tracked = VIEWS.lock().map(|g| g.len() as u32).unwrap_or(0);
    CloudStats {
        tracked,
        om_calls: OM_CALLS.load(Ordering::Relaxed),
        scene_binds: SCENE_BINDS.load(Ordering::Relaxed),
        shaped: SHAPED.load(Ordering::Relaxed),
        merges: MERGES.load(Ordering::Relaxed),
        redirects: REDIRECTS.load(Ordering::Relaxed),
        refusals: REFUSALS.load(Ordering::Relaxed),
        posted: POSTED.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
    }
}

/// The stream should leave colour to the post-merge copy.
pub fn color_owned_by_merge() -> bool {
    COLOR_OWNED.load(Ordering::Acquire)
}

/// True while this thread is recording the post-merge copy. The barrier hook
/// forwards straight through so the copy's own transitions do not recurse.
pub fn copying() -> bool {
    COPYING.with(|c| c.get())
}

/// This barrier is the redirected colour leaving the cloud pass.
pub fn barrier_ends_pass(list: *mut std::ffi::c_void, resource: usize) -> bool {
    if list.is_null() || resource == 0 || copying() {
        return false;
    }
    let Ok(lists) = LISTS.lock() else {
        return false;
    };
    lists
        .iter()
        .any(|s| s.list == list as usize && s.pending.is_some_and(|p| p.resource == resource))
}

/// Record the redirected colour, if this list still has one. `state_now` is the
/// resource state at this point in the command list.
///
/// # Safety
/// `list` must be a command list that is still recording.
pub unsafe fn flush_pending(list: *mut std::ffi::c_void, state_now: u32) {
    if list.is_null() || copying() {
        return;
    }
    let Some(view) = take_pending(list) else {
        return;
    };
    struct ClearCopy;
    impl Drop for ClearCopy {
        fn drop(&mut self) {
            COPYING.with(|c| c.set(false));
        }
    }
    COPYING.with(|c| c.set(true));
    let _guard = ClearCopy;
    let ok = unsafe {
        crate::stream::record_merged_color(list, view.resource, view.width, view.height, state_now)
    };
    if ok {
        POSTED.fetch_add(1, Ordering::Relaxed);
    } else {
        MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

/// The cloud pass ended with the colour still bound as a render target.
///
/// # Safety
/// `list` must be a command list that is still recording.
pub unsafe fn flush_target(list: *mut std::ffi::c_void) {
    unsafe { flush_pending(list, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32) }
}

/// The list was reset. Its recorded draws will not execute.
pub fn drop_pending(list: *mut std::ffi::c_void) {
    if list.is_null() {
        return;
    }
    let Ok(mut lists) = LISTS.lock() else {
        return;
    };
    if let Some(state) = lists.iter_mut().find(|s| s.list == list as usize) {
        state.pending = None;
    }
}

/// Record a render-target view the device just created, when it views a
/// nose-sized cloud-merge target. Other views are ignored.
pub fn note_created(
    device: *mut std::ffi::c_void,
    resource: *mut std::ffi::c_void,
    desc: *const D3D12_RENDER_TARGET_VIEW_DESC,
    handle: usize,
) {
    let Some(view) = describe(resource, desc) else {
        return;
    };
    note_rtv(device, handle, view);
}

fn note_rtv(device: *mut std::ffi::c_void, handle: usize, view: View) {
    if handle == 0 || !nose_target(view) {
        return;
    }
    remember_device(device);
    let Ok(mut views) = VIEWS.lock() else { return };
    if views.len() >= 4096 && !views.contains_key(&handle) {
        views.clear();
    }
    views.insert(handle, view);
}

/// Drop a command list's latched scene colour. A barrier or reset between the
/// scene bind and the merge means that colour is no longer the bound target.
pub fn forget(list: *mut std::ffi::c_void) {
    if list.is_null() || LATCHED.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Ok(mut lists) = LISTS.lock() else { return };
    let Some(state) = lists.iter_mut().find(|s| s.list == list as usize) else {
        return;
    };
    if state.scene.take().is_some() {
        LATCHED.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Observe one `OMSetRenderTargets`. Always forwards to `orig`. May bind the
/// nose scene colour over the merge's unread target 0.
///
/// The second bind, when it happens, calls `orig` rather than the hooked
/// vtable, so it does not re-enter here.
///
/// # Safety
/// `list` must be a recording graphics command list. `orig` is the unhooked method.
pub unsafe fn on_om(
    list: *mut std::ffi::c_void,
    count: u32,
    handles: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    contiguous: i32,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    orig: OmFn,
) {
    // A previous cloud pass has already drawn. Copy its colour before this bind
    // replaces the targets. The redirect below arms a new pending after that.
    unsafe { flush_target(list) };
    OM_CALLS.fetch_add(1, Ordering::Relaxed);
    let prev = scene_of(list);
    let looked = lookup(count, handles, contiguous != 0);
    let views = looked.unwrap_or([None, None, None]);
    let triple = match (count, views[0], views[1], views[2]) {
        (3, Some(a), Some(b), Some(c)) => Some([a, b, c]),
        _ => None,
    };
    let shaped = triple
        .as_ref()
        .is_some_and(|t| merge_shaped(!depth.is_null(), t));
    if shaped {
        SHAPED.fetch_add(1, Ordering::Relaxed);
    }
    let merge = shaped
        && scene_then_merge(
            prev,
            !depth.is_null(),
            triple.as_ref().unwrap_or(&[View::EMPTY; 3]),
        );
    if merge {
        MERGES.fetch_add(1, Ordering::Relaxed);
    }
    unsafe { orig(list, count, handles, contiguous, depth) };
    let mut redirected = None;
    if merge {
        if let Some(scene) = prev {
            if unsafe { redirect(scene, handles, contiguous != 0, orig, list) } {
                REDIRECTS.fetch_add(1, Ordering::Relaxed);
                COLOR_OWNED.store(true, Ordering::Release);
                redirected = Some(scene);
            } else {
                REFUSALS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    // A poisoned view map leaves the latch as it was. An unresolved bind clears it.
    if looked.is_some() {
        let new_scene = next_scene(merge, prev, views[0]);
        if new_scene.is_some() && !merge {
            SCENE_BINDS.fetch_add(1, Ordering::Relaxed);
        }
        set_scene(list, new_scene);
    }
    if let Some(scene) = redirected {
        set_pending(list, scene);
    }
}

unsafe fn redirect(
    scene: View,
    handles: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    contiguous: bool,
    orig: OmFn,
    list: *mut std::ffi::c_void,
) -> bool {
    let Some(device) = DEVICE.lock().ok().and_then(|g| g.clone()) else {
        return false;
    };
    let Some(rtvs) = local_handles(&device) else {
        return false;
    };
    let raw = scene.resource as *mut std::ffi::c_void;
    let Some(resource) = (unsafe { ID3D12Resource::from_raw_borrowed(&raw) }) else {
        return false;
    };
    let desc = D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT(scene.format as i32),
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_RTV {
                MipSlice: 0,
                PlaneSlice: 0,
            },
        },
    };
    // The windows binding types this slot as returning nothing. The real method
    // returns HRESULT; read that so a failed view is not bound.
    let create: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *const D3D12_RENDER_TARGET_VIEW_DESC,
        D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> windows::core::HRESULT =
        unsafe { core::mem::transmute(Interface::vtable(&device).CreateRenderTargetView) };
    let hr = unsafe {
        create(
            Interface::as_raw(&device),
            Interface::as_raw(resource),
            &desc,
            rtvs[0],
        )
    };
    if hr.is_err() {
        return false;
    }
    let incr = RTV_INCR.load(Ordering::Relaxed);
    if incr == 0 || handles.is_null() {
        return false;
    }
    for i in 1..3 {
        let src = unsafe { src_handle(handles, i, contiguous, incr) };
        if src == 0 {
            return false;
        }
        unsafe {
            device.CopyDescriptorsSimple(
                1,
                rtvs[i as usize],
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src },
                D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            );
        }
    }
    unsafe { orig(list, 3, rtvs.as_ptr(), 0, std::ptr::null()) };
    true
}

fn local_handles(device: &ID3D12Device) -> Option<[D3D12_CPU_DESCRIPTOR_HANDLE; 3]> {
    HEAP.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 3,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            };
            let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&desc) }.ok()?;
            let base = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
            let incr =
                unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }
                    as usize;
            if incr == 0 || base.ptr == 0 {
                return None;
            }
            RTV_INCR.store(incr, Ordering::Release);
            *slot = Some(LocalHeap {
                _heap: heap,
                handles: [
                    base,
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: base.ptr + incr,
                    },
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: base.ptr + 2 * incr,
                    },
                ],
            });
        }
        slot.as_ref().map(|h| h.handles)
    })
}

unsafe fn src_handle(
    handles: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    index: u32,
    contiguous: bool,
    incr: usize,
) -> usize {
    unsafe {
        if contiguous {
            (*handles).ptr + index as usize * incr
        } else {
            (*handles.add(index as usize)).ptr
        }
    }
}

/// `None` means the view map could not be read (leave the latch alone).
/// `Some` is the first three targets; missing entries are unknown handles.
fn lookup(
    count: u32,
    handles: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    contiguous: bool,
) -> Option<[Option<View>; 3]> {
    if count > 8 {
        return Some([None, None, None]);
    }
    if count == 0 || handles.is_null() {
        return Some([None, None, None]);
    }
    let incr = RTV_INCR.load(Ordering::Relaxed);
    if contiguous && incr == 0 {
        return None;
    }
    let Ok(views) = VIEWS.lock() else { return None };
    let n = count.min(3);
    let mut out = [None; 3];
    for i in 0..n {
        let handle = unsafe { src_handle(handles, i, contiguous, incr) };
        out[i as usize] = views.get(&handle).copied();
    }
    Some(out)
}

fn scene_of(list: *mut std::ffi::c_void) -> Option<View> {
    if list.is_null() || LATCHED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let Ok(lists) = LISTS.lock() else { return None };
    lists
        .iter()
        .find(|s| s.list == list as usize)
        .and_then(|s| s.scene)
}

fn set_scene(list: *mut std::ffi::c_void, scene: Option<View>) {
    if list.is_null() {
        return;
    }
    let Ok(mut lists) = LISTS.lock() else { return };
    if let Some(state) = lists.iter_mut().find(|s| s.list == list as usize) {
        let had = state.scene.is_some();
        let has = scene.is_some();
        state.scene = scene;
        if had != has {
            if has {
                LATCHED.fetch_add(1, Ordering::Relaxed);
            } else {
                LATCHED.fetch_sub(1, Ordering::Relaxed);
            }
        }
        return;
    }
    if scene.is_none() {
        return;
    }
    if lists.len() >= 128 {
        lists.clear();
        LATCHED.store(0, Ordering::Relaxed);
    }
    lists.push(ListState {
        list: list as usize,
        scene,
        pending: None,
    });
    LATCHED.fetch_add(1, Ordering::Relaxed);
}

fn set_pending(list: *mut std::ffi::c_void, view: View) {
    if list.is_null() || view.resource == 0 {
        return;
    }
    let Ok(mut lists) = LISTS.lock() else {
        return;
    };
    if let Some(state) = lists.iter_mut().find(|s| s.list == list as usize) {
        state.pending = Some(view);
        return;
    }
    if lists.len() >= 128 {
        lists.clear();
        LATCHED.store(0, Ordering::Relaxed);
    }
    lists.push(ListState {
        list: list as usize,
        scene: None,
        pending: Some(view),
    });
}

fn take_pending(list: *mut std::ffi::c_void) -> Option<View> {
    let Ok(mut lists) = LISTS.lock() else {
        return None;
    };
    lists
        .iter_mut()
        .find(|s| s.list == list as usize)
        .and_then(|s| s.pending.take())
}

fn remember_device(device: *mut std::ffi::c_void) {
    if device.is_null() {
        return;
    }
    let Ok(mut slot) = DEVICE.lock() else { return };
    if slot.is_some() {
        return;
    }
    let raw = device;
    if let Some(dev) = unsafe { ID3D12Device::from_raw_borrowed(&raw) } {
        let incr = unsafe { dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }
            as usize;
        if incr != 0 {
            RTV_INCR.store(incr, Ordering::Release);
        }
        *slot = Some(dev.clone());
    }
}

fn describe(
    resource: *mut std::ffi::c_void,
    desc: *const D3D12_RENDER_TARGET_VIEW_DESC,
) -> Option<View> {
    if resource.is_null() {
        return None;
    }
    let (fmt_hint, mip) = if desc.is_null() {
        (0, 0)
    } else {
        let d = unsafe { &*desc };
        let mip = mip_of(d)?;
        if mip != 0 {
            return None;
        }
        (d.Format.0 as u32, mip)
    };
    if fmt_hint != 0 && !cloud_format(fmt_hint) {
        return None;
    }
    let raw = resource;
    let res = unsafe { ID3D12Resource::from_raw_borrowed(&raw) }?;
    let rd = unsafe { res.GetDesc() };
    if rd.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || rd.SampleDesc.Count > 1 {
        return None;
    }
    let format = if fmt_hint == 0 {
        rd.Format.0 as u32
    } else {
        fmt_hint
    };
    if !cloud_format(format) {
        return None;
    }
    let width = rd.Width as u32;
    let height = rd.Height;
    if width != NOSE_W || height != NOSE_H {
        return None;
    }
    Some(View {
        resource: resource as usize,
        format,
        width,
        height,
        mip,
    })
}

fn mip_of(desc: &D3D12_RENDER_TARGET_VIEW_DESC) -> Option<u32> {
    if desc.ViewDimension == D3D12_RTV_DIMENSION_TEXTURE2D
        || desc.ViewDimension == D3D12_RTV_DIMENSION_TEXTURE2DARRAY
    {
        Some(unsafe { desc.Anonymous.Texture2D.MipSlice })
    } else {
        None
    }
}

fn cloud_format(format: u32) -> bool {
    format == FMT_COLOR || format == FMT_SNORM || format == FMT_UNORM
}

fn nose_target(view: View) -> bool {
    view.resource != 0
        && view.mip == 0
        && view.width == NOSE_W
        && view.height == NOSE_H
        && cloud_format(view.format)
}

fn is_nose_colour(view: &View) -> bool {
    view.format == FMT_COLOR && nose_target(*view)
}

/// What the latch holds after this bind. A merge keeps the scene colour so a
/// following merge can still see it. Any other bind keeps target 0 only when
/// that target is the nose scene colour.
fn next_scene(merge: bool, prev: Option<View>, rt0: Option<View>) -> Option<View> {
    if merge {
        prev.filter(is_nose_colour)
    } else {
        rt0.filter(is_nose_colour)
    }
}

/// True when `current` is the three-target nose-sized merge, ignoring what was bound before it.
pub fn merge_shaped(depth: bool, current: &[View]) -> bool {
    if depth || current.len() != 3 {
        return false;
    }
    let color = current[0];
    color.resource != 0
        && color.mip == 0
        && color.format == FMT_COLOR
        && color.width == NOSE_W
        && color.height == NOSE_H
        && current[1].resource != 0
        && current[1].format == FMT_SNORM
        && current[1].width == color.width
        && current[1].height == color.height
        && current[2].resource != 0
        && current[2].format == FMT_UNORM
        && current[2].width == color.width
        && current[2].height == color.height
}

/// True when `current` is the unread nose cloud merge that follows `prev`.
pub fn scene_then_merge(prev: Option<View>, depth: bool, current: &[View]) -> bool {
    let Some(prev) = prev else { return false };
    if !merge_shaped(depth, current) || !is_nose_colour(&prev) {
        return false;
    }
    current[0].resource != prev.resource
}

impl View {
    const EMPTY: View = View {
        resource: 0,
        format: 0,
        width: 0,
        height: 0,
        mip: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(resource: usize, format: u32) -> View {
        View {
            resource,
            format,
            width: NOSE_W,
            height: NOSE_H,
            mip: 0,
        }
    }

    #[test]
    fn merge_follows_a_different_nose_colour() {
        let prev = view(1, FMT_COLOR);
        let current = [view(2, FMT_COLOR), view(3, FMT_SNORM), view(4, FMT_UNORM)];
        assert!(scene_then_merge(Some(prev), false, &current));
        assert!(merge_shaped(false, &current));
    }

    #[test]
    fn main_view_already_targets_its_scene_colour() {
        let prev = view(7, FMT_COLOR);
        let current = [view(7, FMT_COLOR), view(3, FMT_SNORM), view(4, FMT_UNORM)];
        assert!(!scene_then_merge(Some(prev), false, &current));
    }

    #[test]
    fn depth_or_wrong_size_is_not_the_merge() {
        let prev = view(1, FMT_COLOR);
        let current = [view(2, FMT_COLOR), view(3, FMT_SNORM), view(4, FMT_UNORM)];
        assert!(!scene_then_merge(Some(prev), true, &current));
        assert!(!merge_shaped(true, &current));
        let mut tall = prev;
        tall.height = 1262;
        assert!(!scene_then_merge(Some(tall), false, &current));
    }

    #[test]
    fn shaped_merge_without_a_previous_colour_is_not_redirected() {
        let current = [view(2, FMT_COLOR), view(3, FMT_SNORM), view(4, FMT_UNORM)];
        assert!(merge_shaped(false, &current));
        assert!(!scene_then_merge(None, false, &current));
    }

    #[test]
    fn latch_keeps_the_scene_across_the_merge_and_drops_it_after() {
        let scene = view(1, FMT_COLOR);
        let aux = view(9, FMT_SNORM);
        assert_eq!(next_scene(true, Some(scene), Some(aux)), Some(scene));
        assert_eq!(next_scene(false, Some(scene), Some(aux)), None);
        assert_eq!(next_scene(false, Some(scene), Some(scene)), Some(scene));
        assert_eq!(next_scene(false, Some(scene), None), None);
    }
}
