//! One-shot diagnostic capture and the copy helpers shared with the live stream.
//!
//! The **layer scan** copies every shader-readable render target of a given size
//! seen within one frame into a single `RTGB` container. It is how a new
//! viewport's targets are identified (which RT is lit colour, depth, normals…)
//! before the stream is pointed at them. Copies are recorded into the sim's own
//! command list at the barrier where the target becomes readable, with balanced
//! transitions, and read back a few presents later.
//!
//! [`tick`] runs once per present and also drives [`crate::stream`].

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_READBACK,
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0,
    D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, D3D12_RESOURCE_BARRIER_FLAG_NONE,
    D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATES,
    D3D12_RESOURCE_TRANSITION_BARRIER, D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

/// Result of the last scan, for the service thread to log.
static DUMP_RESULT: Mutex<Option<String>> = Mutex::new(None);
/// Our own present counter; readbacks are read a few frames after the copy was
/// recorded, by when the sim's GPU has certainly executed it.
static CAPTURE_FRAME: AtomicU64 = AtomicU64::new(0);
/// Frames to wait after recording before reading back.
const READBACK_LATENCY: u64 = 3;

/// A recorded layer awaiting read-back.
struct LayerReadback {
    buf: ID3D12Resource,
    row_pitch: u32, // readback stride (GPU padded)
    row_bytes: u32, // tight bytes per row (padding stripped on read-back)
    w: u32,
    h: u32,
    format: u32,
    role: u32,
}

/// A queued layer scan.
struct ScanJob {
    path: String,
    width: u32,
    height: u32,
    /// Resource pointers already captured this scan (dedupe; one RT transitions
    /// to readable several times a frame). Doubles as the reservation/cap count.
    seen: Vec<usize>,
    /// Captured layers (role = capture order).
    caps: Vec<LayerReadback>,
    /// Hard cap on captures so a busy size can't run away.
    max: usize,
}
static SCAN_JOB: Mutex<Option<ScanJob>> = Mutex::new(None);
/// Frame by which collection stops (0 = inactive). Collects over a couple of
/// presents so it spans a full frame of barriers.
static SCAN_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// Frame at which to read the scanned layers back (0 = none pending).
static SCAN_READY_FRAME: AtomicU64 = AtomicU64::new(0);
/// Presents to collect over before reading back.
const SCAN_WINDOW: u64 = 2;

/// Request a one-shot scan of every readable RT at `width`×`height`.
pub fn request_layer_scan(path: String, width: u32, height: u32) {
    if let Ok(mut g) = SCAN_JOB.lock() {
        *g = Some(ScanJob {
            path,
            width,
            height,
            seen: Vec::new(),
            caps: Vec::new(),
            max: 64,
        });
    }
    SCAN_READY_FRAME.store(0, Ordering::Release);
    SCAN_DEADLINE.store(
        CAPTURE_FRAME.load(Ordering::Acquire) + SCAN_WINDOW,
        Ordering::Release,
    );
}

/// Whether a layer scan is collecting (cheap gate for the barrier hook).
pub fn scan_pending() -> bool {
    SCAN_DEADLINE.load(Ordering::Acquire) != 0
}

/// If a scan is active and `resource` matches its size and hasn't been captured
/// yet, copy it. Claim under a brief lock, record lock-free, store under a brief
/// lock (holding it across the D3D work would convoy the sim's render threads).
///
/// # Safety
/// `cmdlist` must be a valid, recording `ID3D12GraphicsCommandList`.
pub unsafe fn record_scan_layer(
    cmdlist: *mut c_void,
    resource: &ID3D12Resource,
    desc: &D3D12_RESOURCE_DESC,
    state_after_raw: u32,
) {
    let (w, h) = (desc.Width as u32, desc.Height);
    let ptr = resource.as_raw() as usize;

    // Phase 1 — reserve (dedupe + cap) under a brief lock.
    {
        let Ok(mut guard) = SCAN_JOB.lock() else {
            return;
        };
        let Some(job) = guard.as_mut() else { return };
        if w != job.width || h != job.height || job.seen.len() >= job.max || job.seen.contains(&ptr)
        {
            return;
        }
        job.seen.push(ptr);
    }

    // Phase 2 — record the copy lock-free.
    let state_after = D3D12_RESOURCE_STATES(state_after_raw as i32);
    let result = unsafe { record_layer_inner(cmdlist, resource, desc, state_after, 0) };

    // Phase 3 — store the readback (role = capture order).
    let Ok(mut guard) = SCAN_JOB.lock() else {
        return;
    };
    let Some(job) = guard.as_mut() else { return };
    match result {
        Ok(mut rb) => {
            rb.role = job.caps.len() as u32;
            job.caps.push(rb);
        }
        Err(e) => set_result(format!("scan layer (fmt {}) failed: {e}", desc.Format.0)),
    }
}

/// Take the last scan result message, if any (called from the service thread).
pub fn take_result() -> Option<String> {
    DUMP_RESULT.lock().ok().and_then(|mut g| g.take())
}

fn set_result(msg: String) {
    if let Ok(mut g) = DUMP_RESULT.lock() {
        *g = Some(msg);
    }
}

unsafe fn record_layer_inner(
    cmdlist: *mut c_void,
    resource: &ID3D12Resource,
    desc: &D3D12_RESOURCE_DESC,
    state_after: D3D12_RESOURCE_STATES,
    role: u32,
) -> Result<LayerReadback, String> {
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
        let readback = create_readback_buffer(&device, total)?;
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

        Ok(LayerReadback {
            buf: readback,
            row_pitch: fp.Footprint.RowPitch,
            row_bytes: row_bytes as u32,
            w: desc.Width as u32,
            h: desc.Height,
            format: desc.Format.0 as u32,
            role,
        })
    }
}

/// Called once per present: advance the frame counter, drive the live stream,
/// and finish a layer scan once its copies have executed.
pub fn tick() {
    let frame = CAPTURE_FRAME.fetch_add(1, Ordering::AcqRel) + 1;

    // Continuous stream: read back the ready frame and publish it, then rotate
    // the ring. Cheap and a no-op when not streaming.
    unsafe { crate::stream::stream_tick(frame) };

    // Layer scan: stop collecting at the deadline, schedule the read-back, then
    // write every captured layer to one container.
    let sdead = {
        let d = SCAN_DEADLINE.load(Ordering::Acquire);
        d != 0 && frame >= d
    };
    if sdead {
        SCAN_DEADLINE.store(0, Ordering::Release);
        SCAN_READY_FRAME.store(frame + READBACK_LATENCY, Ordering::Release);
    }
    let sready = SCAN_READY_FRAME.load(Ordering::Acquire);
    if sready != 0 && frame >= sready {
        SCAN_READY_FRAME.store(0, Ordering::Release);
        if let Some(job) = SCAN_JOB.lock().ok().and_then(|mut g| g.take()) {
            let msg = unsafe { write_scan(&job) };
            set_result(msg);
        }
    }
}

/// Map every scanned layer and write one container file ("RTGB": magic, version,
/// layer count, then per layer {role, format, w, h, byte_len, tight bytes};
/// role = capture order). The consumer renders each layer
/// format-aware so we can eyeball which 1024×576 RT is which.
unsafe fn write_scan(job: &ScanJob) -> String {
    unsafe {
        let mut out = Vec::new();
        out.extend_from_slice(b"RTGB");
        out.extend_from_slice(&1u32.to_le_bytes()); // version
        out.extend_from_slice(&(job.caps.len() as u32).to_le_bytes());

        let mut got = Vec::new();
        for rb in &job.caps {
            let (w, h, pitch, rowb) = (
                rb.w as usize,
                rb.h as usize,
                rb.row_pitch as usize,
                rb.row_bytes as usize,
            );
            let mut ptr: *mut c_void = std::ptr::null_mut();
            if rb.buf.Map(0, None, Some(&mut ptr)).is_err() || ptr.is_null() {
                continue;
            }
            let mut bytes = Vec::with_capacity(rowb * h);
            for y in 0..h {
                let row = std::slice::from_raw_parts(ptr.add(y * pitch) as *const u8, rowb);
                bytes.extend_from_slice(row);
            }
            rb.buf.Unmap(0, None);
            out.extend_from_slice(&rb.role.to_le_bytes());
            out.extend_from_slice(&rb.format.to_le_bytes());
            out.extend_from_slice(&(w as u32).to_le_bytes());
            out.extend_from_slice(&(h as u32).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&bytes);
            got.push(format!("#{} fmt{}", rb.role, rb.format));
        }

        let path = resolve_path(&job.path, "gbuf");
        if let Err(e) = std::fs::write(&path, &out) {
            return format!("scan write {}: {e}", path.display());
        }
        format!(
            "scanned {} layer(s) at {}x{} [{}] -> {}",
            job.caps.len(),
            job.width,
            job.height,
            got.join(", "),
            path.display()
        )
    }
}

/// Resolve `path` to a writable absolute file, defaulting relative names into the
/// temp dir and ensuring the given extension.
fn resolve_path(path: &str, ext: &str) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::from(path);
    if out.extension().is_none() {
        out.set_extension(ext);
    }
    if out.is_relative() {
        // Sanitize the host-supplied final component: characters illegal in
        // Windows filenames (notably ':', which would otherwise create an NTFS
        // alternate data stream, hiding the output and dropping the extension).
        if let Some(name) = out.file_name().map(|n| {
            n.to_string_lossy()
                .replace([':', '*', '?', '"', '<', '>', '|'], "_")
        }) {
            out.set_file_name(name);
        }
        out = std::env::temp_dir().join(out);
    }
    out
}

/// A readback (CPU-readable) buffer of `size` bytes, in COPY_DEST state.
pub(crate) unsafe fn create_readback_buffer(
    device: &ID3D12Device,
    size: u64,
) -> Result<ID3D12Resource, String> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(0),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: Default::default(),
    };
    let mut res: Option<ID3D12Resource> = None;
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut res,
            )
            .map_err(|e| format!("CreateCommittedResource(readback): {e}"))?;
    }
    res.ok_or_else(|| "readback resource is null".into())
}

/// Record a transition barrier and reclaim the resource reference it clones.
pub(crate) unsafe fn barrier(
    list: &ID3D12GraphicsCommandList,
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let b = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(res.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe { list.ResourceBarrier(std::slice::from_ref(&b)) };
    // Reclaim the cloned ref so the resource's ref-count stays balanced.
    let t = unsafe { ManuallyDrop::into_inner(b.Anonymous.Transition) };
    drop(ManuallyDrop::into_inner(t.pResource));
}
