//! Continuous viewport streaming for the live FLIR tuner.
//!
//! Unlike the one-shot layer scan in [`crate::capture`], this copies a viewport's
//! layers (depth + a selected colour RT) **every frame** and publishes them to
//! the host through the [`VideoBlock`] shared mapping. The copies are injected
//! into the sim's own command lists at the barrier where each resource becomes
//! shader-readable (same mechanism as the scan), read back a few presents
//! later once the sim's GPU has executed them, then written into the mapping's
//! triple-buffered seqlock.
//!
//! Lock discipline matches [`crate::capture::record_scan_layer`]: the global
//! mutex is held only to claim a layer and to store its result, never across a
//! device call — holding it across `CreateCommittedResource` once convoyed every
//! render worker thread and froze presents.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_STATE_COPY_SOURCE,
    D3D12_RESOURCE_STATES, D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::core::Interface;

use infinity_viewports_protocol::{
    VIDEO_LAYER_BYTES, VIDEO_MAGIC, VIDEO_SLOTS, VIDEO_VERSION, VideoBlock, VideoLayerDesc,
};

use crate::capture::{barrier, create_readback_buffer};

/// Ring depth — must exceed the read-back latency so the slot being read is
/// never the slot being recorded.
const RING: usize = 4;
/// Presents to wait after recording before the sim's GPU has executed the copy.
const LATENCY: u64 = 3;
/// Layer indices within a slot.
const L_DEPTH: usize = 0;
const L_COLOR: usize = 1;
const NLAYERS: usize = 2;
/// Roles written into the container (match `crate::capture` roles).
const ROLE_DEPTH: u32 = 0;
const ROLE_COLOR: u32 = 2;
/// The nose depth proxy is the R32_FLOAT linear-depth RT.
const DEPTH_FMT: u32 = 41;

/// Metadata of one recorded layer, pending read-back.
#[derive(Clone, Copy)]
struct RecMeta {
    role: u32,
    fmt: u32,
    w: u32,
    h: u32,
    row_pitch: u32,
    row_bytes: u32,
}

struct StreamState {
    on: bool,
    /// `*mut VideoBlock` as usize (raw pointer isn't `Send`).
    video: usize,
    w: u32,
    h: u32,
    /// Which colour RT to stream: DXGI format + which occurrence this frame.
    color_fmt: u32,
    color_occ: u32,
    /// Persistent readback buffers, indexed `slot*NLAYERS + layer`.
    bufs: Vec<Option<ID3D12Resource>>,
    /// Recorded metadata this round (None until recorded).
    rec: Vec<Option<RecMeta>>,
    /// A layer claimed (being recorded) but not yet stored.
    claimed: Vec<bool>,
    /// Which frame each ring slot is recording.
    frame_of: Vec<u64>,
    /// Ring slot barriers record into this frame.
    cur: usize,
    /// Colour-format resources seen so far this frame (for occurrence select).
    color_seen: u32,
    /// Frames published to the mapping (diagnostics).
    published: u64,
}

impl StreamState {
    const fn idle() -> StreamState {
        StreamState {
            on: false,
            video: 0,
            w: 0,
            h: 0,
            color_fmt: 26,
            color_occ: 0,
            bufs: Vec::new(),
            rec: Vec::new(),
            claimed: Vec::new(),
            frame_of: Vec::new(),
            cur: 0,
            color_seen: 0,
            published: 0,
        }
    }
}

static STREAM: Mutex<StreamState> = Mutex::new(StreamState::idle());

/// Read-back rings retired by a size change. Never released: the sim's
/// command lists copy into them up to [`RING`] frames after recording, with
/// no fence of ours to say when the GPU is done, and a buffer freed under a
/// pending copy hangs the device (`DXGI_ERROR_DEVICE_HUNG`). A few MB per
/// size change, and the bridge leaks its GPU objects at unload for the same
/// reason.
static RETIRED: Mutex<Vec<ID3D12Resource>> = Mutex::new(Vec::new());

/// Start streaming `width`×`height` into the `VideoBlock` at `video_ptr`.
/// `video_ptr` must point to a mapped, zero-filled `VideoBlock` that outlives the
/// stream (owned by the bridge).
///
/// A restart at the same size reuses the read-back ring of the last stream.
pub fn start_stream(video_ptr: usize, width: u32, height: u32) {
    let Ok(mut st) = STREAM.lock() else { return };
    let mut bufs = std::mem::take(&mut st.bufs);
    if width != st.w || height != st.h || bufs.len() != RING * NLAYERS {
        retire(&mut bufs);
        bufs = (0..RING * NLAYERS).map(|_| None).collect();
    }
    let (color_fmt, color_occ) = (st.color_fmt, st.color_occ);
    *st = StreamState::idle();
    st.color_fmt = color_fmt;
    st.color_occ = color_occ;
    st.on = true;
    st.video = video_ptr;
    st.w = width;
    st.h = height;
    st.bufs = bufs;
    st.rec = (0..RING * NLAYERS).map(|_| None).collect();
    st.claimed = vec![false; RING * NLAYERS];
    st.frame_of = vec![u64::MAX; RING];
    // Stamp the mapping header (seq stays 0 until the first full frame).
    if video_ptr != 0 {
        unsafe {
            let vb = &*(video_ptr as *const VideoBlock);
            vb.header.magic.store(VIDEO_MAGIC, Ordering::Relaxed);
            vb.header.version.store(VIDEO_VERSION, Ordering::Relaxed);
            vb.header.width.store(width, Ordering::Relaxed);
            vb.header.height.store(height, Ordering::Relaxed);
            vb.header
                .layer_count
                .store(NLAYERS as u32, Ordering::Relaxed);
            vb.header.seq.store(0, Ordering::Release);
        }
    }
}

/// Stop streaming. Nothing more is recorded or published; the read-back
/// ring stays allocated, because copies recorded in the last few frames may
/// still be in flight on the GPU (see [`RETIRED`]).
pub fn stop_stream() {
    if let Ok(mut st) = STREAM.lock() {
        st.on = false;
        st.video = 0;
        st.rec.iter_mut().for_each(|r| *r = None);
        st.claimed.iter_mut().for_each(|c| *c = false);
    }
}

fn retire(bufs: &mut Vec<Option<ID3D12Resource>>) {
    let old: Vec<ID3D12Resource> = bufs.drain(..).flatten().collect();
    if old.is_empty() {
        return;
    }
    match RETIRED.lock() {
        Ok(mut r) => r.extend(old),
        Err(_) => old.into_iter().for_each(std::mem::forget),
    }
}

/// Select which colour RT to stream (format + occurrence index within a frame).
pub fn set_color_source(format: u32, occurrence: u32) {
    if let Ok(mut st) = STREAM.lock() {
        st.color_fmt = format;
        st.color_occ = occurrence;
    }
}

/// Whether streaming is active (cheap gate for the barrier hook).
pub fn stream_active() -> bool {
    STREAM.lock().map(|s| s.on).unwrap_or(false)
}

/// Published frame count (diagnostics).
pub fn published_frames() -> u64 {
    STREAM.lock().map(|s| s.published).unwrap_or(0)
}

/// If this readable resource is a layer we're streaming and not yet captured
/// this frame, record a copy into the sim's command list `cmdlist`.
///
/// # Safety
/// `cmdlist` must be a valid, recording `ID3D12GraphicsCommandList`.
pub unsafe fn record_stream_layer(
    cmdlist: *mut c_void,
    resource: &ID3D12Resource,
    desc: &D3D12_RESOURCE_DESC,
    state_after_raw: u32,
) {
    let fmt = desc.Format.0 as u32;
    let (w, h) = (desc.Width as u32, desc.Height);

    // Phase 1 — classify + claim under a brief lock. Clone the existing buffer
    // (if any) so phase 2 can record without the lock.
    let (slot, li, existing) = {
        let Ok(mut st) = STREAM.lock() else { return };
        if !st.on || w != st.w || h != st.h {
            return;
        }
        let li = if fmt == DEPTH_FMT {
            L_DEPTH
        } else if fmt == st.color_fmt {
            // Once the cloud merge is redirected, colour is copied at the end of
            // that pass. An earlier readable transition is still the pre-cloud scene.
            if crate::cloud::color_owned_by_merge() {
                return;
            }
            let occ = st.color_seen;
            st.color_seen += 1;
            if occ != st.color_occ {
                return;
            }
            L_COLOR
        } else {
            return;
        };
        let slot = st.cur;
        let idx = slot * NLAYERS + li;
        if st.claimed[idx] || st.rec[idx].is_some() {
            return;
        }
        st.claimed[idx] = true;
        (slot, li, st.bufs[idx].clone())
    };

    // Phase 2 — ensure a buffer, then record the copy lock-free.
    let idx = slot * NLAYERS + li;
    let role = if li == L_DEPTH {
        ROLE_DEPTH
    } else {
        ROLE_COLOR
    };
    let result = unsafe {
        record_into(
            cmdlist,
            resource,
            desc,
            D3D12_RESOURCE_STATES(state_after_raw as i32),
            existing,
            role,
        )
    };

    // Phase 3 — store the result (or release the claim).
    let Ok(mut st) = STREAM.lock() else { return };
    // The round may have rotated while we recorded; only store if still current.
    if st.cur != slot {
        st.claimed[idx] = false;
        return;
    }
    match result {
        Ok((buf, meta)) => {
            st.bufs[idx] = Some(buf);
            st.rec[idx] = Some(meta);
        }
        Err(_) => {
            st.claimed[idx] = false;
        }
    }
}

/// Record a copy of `resource` into `cmdlist`, reusing `existing` readback buffer
/// if present (else creating one). Returns the buffer (to cache) and metadata.
unsafe fn record_into(
    cmdlist: *mut c_void,
    resource: &ID3D12Resource,
    desc: &D3D12_RESOURCE_DESC,
    state_after: D3D12_RESOURCE_STATES,
    existing: Option<ID3D12Resource>,
    role: u32,
) -> Result<(ID3D12Resource, RecMeta), String> {
    unsafe {
        let mut dev: Option<ID3D12Device> = None;
        resource
            .GetDevice(&mut dev)
            .map_err(|e| format!("GetDevice: {e}"))?;
        let device = dev.ok_or("resource has no device")?;
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut row_bytes = 0u64;
        let mut total = 0u64;
        device.GetCopyableFootprints(
            desc,
            0,
            1,
            0,
            Some(&mut fp),
            None,
            Some(&mut row_bytes),
            Some(&mut total),
        );
        if (row_bytes as usize) * (desc.Height as usize) > VIDEO_LAYER_BYTES {
            return Err("layer exceeds stream slot".into());
        }
        let readback = match existing {
            Some(b) => b,
            None => create_readback_buffer(&device, VIDEO_LAYER_BYTES as u64)?,
        };
        let list =
            ID3D12GraphicsCommandList::from_raw_borrowed(&cmdlist).ok_or("null command list")?;

        barrier(
            list,
            resource,
            state_after,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(resource.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(readback.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: fp,
            },
        };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        drop(ManuallyDrop::into_inner(src.pResource));
        drop(ManuallyDrop::into_inner(dst.pResource));
        barrier(
            list,
            resource,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            state_after,
        );

        let meta = RecMeta {
            role,
            fmt: desc.Format.0 as u32,
            w: desc.Width as u32,
            h: desc.Height,
            row_pitch: fp.Footprint.RowPitch,
            row_bytes: row_bytes as u32,
        };
        Ok((readback, meta))
    }
}

/// Record the nose colour the cloud merge just wrote. Replaces an earlier colour
/// copy in this frame's slot. The resource is still in `state_now`.
///
/// # Safety
/// `cmdlist` must be a valid, recording `ID3D12GraphicsCommandList`. `resource`
/// must be a live `ID3D12Resource`.
pub unsafe fn record_merged_color(
    cmdlist: *mut c_void,
    resource: usize,
    width: u32,
    height: u32,
    state_now: u32,
) -> bool {
    if cmdlist.is_null() || resource == 0 {
        return false;
    }
    let raw = resource as *mut c_void;
    let Some(res) = (unsafe { ID3D12Resource::from_raw_borrowed(&raw) }) else {
        return false;
    };
    let desc = unsafe { res.GetDesc() };
    if desc.Width as u32 != width || desc.Height != height {
        return false;
    }
    let (slot, existing) = {
        let Ok(mut st) = STREAM.lock() else {
            return false;
        };
        if !st.on || width != st.w || height != st.h {
            return false;
        }
        let idx = st.cur * NLAYERS + L_COLOR;
        if st.claimed.get(idx).copied().unwrap_or(true) {
            return false;
        }
        st.claimed[idx] = true;
        st.rec[idx] = None;
        let existing = st.bufs.get(idx).and_then(|b| b.clone());
        (st.cur, existing)
    };
    let result = unsafe {
        record_into(
            cmdlist,
            res,
            &desc,
            D3D12_RESOURCE_STATES(state_now as i32),
            existing,
            ROLE_COLOR,
        )
    };
    let Ok(mut st) = STREAM.lock() else {
        return false;
    };
    let idx = slot * NLAYERS + L_COLOR;
    if st.cur != slot {
        if idx < st.claimed.len() {
            st.claimed[idx] = false;
        }
        return false;
    }
    match result {
        Ok((buf, meta)) => {
            if idx < st.bufs.len() {
                st.bufs[idx] = Some(buf);
                st.rec[idx] = Some(meta);
                st.claimed[idx] = false;
                true
            } else {
                false
            }
        }
        Err(_) => {
            if idx < st.claimed.len() {
                st.claimed[idx] = false;
            }
            false
        }
    }
}

/// Called once per present (from `capture::tick`). Reads back the slot recorded
/// `LATENCY` frames ago, publishes it to the mapping, then rotates the ring to a
/// fresh slot for the next frame's barriers.
pub unsafe fn stream_tick(frame: u64) {
    // Snapshot what to publish under the lock, then do the heavy map/copy after
    // releasing it so barrier-hook threads are never blocked on the copy.
    let publish: Option<(usize, Vec<(ID3D12Resource, RecMeta)>)> = {
        let Ok(mut st) = STREAM.lock() else { return };
        if !st.on {
            return;
        }
        let out = if frame >= LATENCY {
            let rf = frame - LATENCY;
            let rslot = (rf % RING as u64) as usize;
            if st.frame_of[rslot] == rf {
                let d = st.rec[rslot * NLAYERS + L_DEPTH];
                let c = st.rec[rslot * NLAYERS + L_COLOR];
                match (d, c) {
                    (Some(dm), Some(cm)) => {
                        let db = st.bufs[rslot * NLAYERS + L_DEPTH].clone();
                        let cb = st.bufs[rslot * NLAYERS + L_COLOR].clone();
                        match (db, cb) {
                            (Some(db), Some(cb)) => Some((st.video, vec![(db, dm), (cb, cm)])),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        // Rotate: this frame's barriers record into a fresh slot.
        let cur = (frame % RING as u64) as usize;
        st.cur = cur;
        st.frame_of[cur] = frame;
        for li in 0..NLAYERS {
            st.rec[cur * NLAYERS + li] = None;
            st.claimed[cur * NLAYERS + li] = false;
        }
        st.color_seen = 0;
        if out.is_some() {
            st.published += 1;
        }
        out
    };

    if let Some((video, layers)) = publish {
        unsafe { write_frame(video, &layers) };
    }
}

/// Map each recorded layer, strip the read-back row padding, and write one frame
/// into the next slot of the `VideoBlock`, then release it via `seq`.
unsafe fn write_frame(video: usize, layers: &[(ID3D12Resource, RecMeta)]) {
    if video == 0 {
        return;
    }
    unsafe {
        let base = video as *mut VideoBlock;
        let seq = (*base).header.seq.load(Ordering::Relaxed);
        let wslot = (seq % VIDEO_SLOTS as u64) as usize;
        for (i, (buf, meta)) in layers.iter().enumerate() {
            let offset = i * VIDEO_LAYER_BYTES;
            let dst = core::ptr::addr_of_mut!((*base).slots[wslot]) as *mut u8;
            let (h, pitch, rowb) = (
                meta.h as usize,
                meta.row_pitch as usize,
                meta.row_bytes as usize,
            );
            let mut ptr: *mut c_void = std::ptr::null_mut();
            if buf.Map(0, None, Some(&mut ptr)).is_err() || ptr.is_null() {
                continue;
            }
            for y in 0..h {
                let srow = (ptr as *const u8).add(y * pitch);
                let drow = dst.add(offset + y * rowb);
                std::ptr::copy_nonoverlapping(srow, drow, rowb);
            }
            buf.Unmap(0, None);
            let desc = VideoLayerDesc {
                role: meta.role,
                format: meta.fmt,
                width: meta.w,
                height: meta.h,
                byte_len: (rowb * h) as u32,
                offset: offset as u32,
            };
            core::ptr::write(
                core::ptr::addr_of_mut!((*base).header.descs[wslot][i]),
                desc,
            );
        }
        // Zero any unused descriptor slots for this frame.
        for i in layers.len()..infinity_viewports_protocol::VIDEO_MAX_LAYERS {
            core::ptr::write(
                core::ptr::addr_of_mut!((*base).header.descs[wslot][i]),
                VideoLayerDesc::ZERO,
            );
        }
        (*base)
            .header
            .layer_count
            .store(layers.len() as u32, Ordering::Relaxed);
        (*base).header.seq.store(seq + 1, Ordering::Release);
    }
}
