//! Bridge lifecycle and the service loop.
//!
//! `Bridge::start` creates the shared block and spawns a dedicated service
//! thread (our own thread inside the sim process — never a sim thread). The loop
//! publishes the header, then each tick drains the command ring, reads the
//! latest config, and publishes a status snapshot plus a heartbeat the host
//! watches for liveness. The picture leaves through the video mapping
//! (`stream:*` commands); the thermal develop happens host-side.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use infinity_viewports_gpu::Capturer;
use infinity_viewports_protocol::{
    BridgeState, CommandCode, LogLevel, LogRecord, SharedBlock, StatusCode, StatusSnapshot,
};

use crate::shared;

/// Why the bridge could not start.
#[derive(Clone, Copy, Debug)]
pub enum StartError {
    SharedBlock = 1,
    Thread = 2,
}

pub struct Bridge {
    shutdown: Arc<AtomicBool>,
}

impl Bridge {
    /// Create the shared block and spin up the service thread. Returns once the
    /// thread is running; the thread publishes the header when ready.
    pub fn start(_shared_name: &str) -> Result<Bridge, StartError> {
        let shared = shared::create().ok_or(StartError::SharedBlock)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        // Raw pointers are not Send; hand the address across as usize.
        let addr = shared.ptr as usize;
        let pid = shared.pid;
        std::thread::Builder::new()
            .name("viewports-bridge".into())
            .spawn(move || service_loop(addr as *mut SharedBlock, pid, sd))
            .map_err(|_| StartError::Thread)?;
        Ok(Bridge { shutdown })
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Appends structured records to the bridge→host log ring with a monotonic seq.
struct Logger {
    block: *const SharedBlock,
    seq: u64,
}

impl Logger {
    fn log(&mut self, level: LogLevel, tag: &str, msg: &str) {
        let mut rec = LogRecord::EMPTY;
        rec.seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        rec.level = level as u8;
        let tb = tag.as_bytes();
        let n = tb.len().min(rec.tag.len());
        rec.tag[..n].copy_from_slice(&tb[..n]);
        let mb = msg.as_bytes();
        let n = mb.len().min(rec.msg.len() - 1); // leave a NUL
        rec.msg[..n].copy_from_slice(&mb[..n]);
        // SAFETY: the mapping is valid for the process lifetime; LogRing::push is
        // the single-producer side (only this thread writes logs).
        unsafe { (*self.block).logs.push(rec) };
    }
}

fn service_loop(ptr: *mut SharedBlock, pid: u32, shutdown: Arc<AtomicBool>) {
    // SAFETY: `ptr` is the leaked, process-lifetime mapping created in `start`.
    let block = unsafe { &*ptr };
    let mut logger = Logger { block: ptr, seq: 0 };

    // Initial status before publishing the header (no reader yet).
    let mut status = StatusSnapshot::ZERO;
    status.state_u8 = BridgeState::Starting as u8;
    status.last_error_u16 = StatusCode::NoCameraContract as u16;
    publish_status(ptr, status);

    // Publish the header LAST: this makes `ready` true and the block readable.
    block.header.publish(pid, SharedBlock::SIZE);
    logger.log(
        LogLevel::Info,
        "bridge",
        &format!("bridge live in pid {pid}; protocol handshake published"),
    );

    let mut capturer = Capturer::new();
    match capturer.install() {
        Ok(desc) => logger.log(LogLevel::Info, "gpu", &desc),
        Err(e) => logger.log(LogLevel::Error, "gpu", &format!("present hook failed: {e}")),
    }
    let mut present_logged = false;
    let mut last_inv_len = 0usize;
    // Live video stream: the on-demand video mapping, created on first
    // `stream:start` and cached (process-lifetime) for reuse.
    let mut video_ptr: Option<usize> = None;
    let mut last_stream_log = 0u64;
    // Set by a Teardown command: after the loop, uninstall the hooks and unload.
    let mut teardown = false;

    let mut heartbeat: u64 = 0;
    let mut last_fps = Instant::now();
    let mut fps_frames: u64 = 0;
    let mut fps: f32 = 0.0;

    while !shutdown.load(Ordering::Acquire) {
        // 1. Drain discrete commands (lossless).
        while let Some(cmd) = block.commands.pop() {
            match cmd.code {
                c if c == CommandCode::Shutdown as u32 => {
                    logger.log(LogLevel::Info, "bridge", "shutdown command received");
                    shutdown.store(true, Ordering::Release);
                }
                c if c == CommandCode::Teardown as u32 => {
                    logger.log(
                        LogLevel::Info,
                        "bridge",
                        "teardown command received; will unhook + unload",
                    );
                    teardown = true;
                    shutdown.store(true, Ordering::Release);
                }
                c if c == CommandCode::DumpCapture as u32 => {
                    let path = cstr(&cmd.arg);
                    if path == "inventory" {
                        log_inventory(&mut logger, &capturer);
                    } else if path == "stream:start" {
                        let ptr = *video_ptr
                            .get_or_insert_with(|| shared::create_video(pid).unwrap_or(0));
                        if ptr == 0 {
                            logger.log(
                                LogLevel::Error,
                                "gpu",
                                "stream: failed to create video mapping",
                            );
                        } else {
                            capturer.start_stream(ptr, 1024, 576);
                            logger.log(LogLevel::Info, "gpu", "live stream started (nose 1024x576: depth f41 + colour); open the video mapping host-side");
                        }
                    } else if path == "stream:stop" {
                        capturer.stop_stream();
                        logger.log(LogLevel::Info, "gpu", "live stream stopped");
                    } else if let Some(rest) = path.strip_prefix("stream:src:") {
                        // "stream:src:<fmt>:<occ>" — pick which colour RT to stream.
                        let mut it = rest.split(':');
                        let fmt = it.next().and_then(|s| s.parse::<u32>().ok());
                        let occ = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        if let Some(fmt) = fmt {
                            capturer.set_stream_color_source(fmt, occ);
                            logger.log(
                                LogLevel::Info,
                                "gpu",
                                &format!("stream colour source -> fmt {fmt} occ {occ}"),
                            );
                        }
                    } else if let Some(name) = path.strip_prefix("scan:nose:") {
                        logger.log(LogLevel::Info, "gpu", &format!("nose layer scan queued (every readable RT @1024x576, one frame) -> {name}"));
                        capturer.request_layer_scan(name.to_string(), 1024, 576);
                    } else {
                        logger.log(
                            LogLevel::Warn,
                            "bridge",
                            &format!("unknown capture command {path:?}"),
                        );
                    }
                }
                c if c == CommandCode::Rediscover as u32 => {
                    logger.log(LogLevel::Info, "engine", "rediscover requested");
                }
                _ => {}
            }
        }

        // 1b. Report the GPU foothold once the sim presents its first frame,
        //     then a periodic heartbeat of the command-stream counters so the
        //     command-queue hook is visibly firing.
        if !present_logged {
            let o = capturer.observed();
            if o.present_count > 0 && o.backbuffer_w > 0 {
                logger.log(
                    LogLevel::Info,
                    "gpu",
                    &format!(
                        "Present hook live: backbuffer {}x{} dxgi_format={} after {} presents",
                        o.backbuffer_w, o.backbuffer_h, o.backbuffer_format, o.present_count
                    ),
                );
                present_logged = true;
            }
        } else if heartbeat % 300 == 0 {
            let o = capturer.observed();
            let per_frame = if o.present_count > 0 {
                o.ecl_lists as f64 / o.present_count as f64
            } else {
                0.0
            };
            logger.log(LogLevel::Debug, "gpu", &format!("stream: {} presents, {} ECL calls, {} lists (~{:.1}/frame), {} ResourceBarrier calls", o.present_count, o.ecl_submits, o.ecl_lists, per_frame, capturer.resource_barrier_calls()));
            let c = infinity_viewports_gpu::cloud::stats();
            logger.log(
                LogLevel::Debug,
                "gpu",
                &format!(
                    "clouds: tracked {} om {} scene {} shaped {} merges {} redirects {} refusals {} posted {} misses {}",
                    c.tracked, c.om_calls, c.scene_binds, c.shaped, c.merges, c.redirects, c.refusals, c.posted, c.misses
                ),
            );
            // Re-log the RT inventory whenever it has grown (e.g. after the user
            // changes resolution, forcing MSFS to reallocate its targets).
            let inv = capturer.rt_inventory();
            if inv.len() != last_inv_len {
                last_inv_len = inv.len();
                log_inventory(&mut logger, &capturer);
            }
        }

        // 1c. Surface a completed layer scan.
        if let Some(msg) = capturer.take_dump_result() {
            logger.log(LogLevel::Info, "gpu", &msg);
        }

        // 2. Latest config (bounded seqlock read).
        let cfg = read_config(ptr);
        let applied_gen = cfg.map(|c| c.generation).unwrap_or(0);
        let enabled = cfg.map(|c| c.enabled != 0).unwrap_or(false);

        // 3. Running = the stream is publishing frames to the video mapping.
        let frames = capturer.stream_published_frames();
        let streaming = video_ptr.is_some() && capturer.stream_active();

        // 4. Publish status + heartbeat.
        heartbeat = heartbeat.wrapping_add(1);
        if last_fps.elapsed() >= Duration::from_secs(1) {
            fps = frames.saturating_sub(fps_frames) as f32 / last_fps.elapsed().as_secs_f32();
            fps_frames = frames;
            last_fps = Instant::now();
            // While streaming, report how many frames have reached the mapping so
            // the host side can tell the pipe is live (and the colour source hit).
            if video_ptr.is_some() && frames != last_stream_log {
                logger.log(
                    LogLevel::Info,
                    "gpu",
                    &format!("stream: {frames} frames published"),
                );
                last_stream_log = frames;
            }
        }
        let mut st = StatusSnapshot::ZERO;
        st.generation = heartbeat;
        st.applied_config_generation = applied_gen;
        st.state_u8 = state_for(enabled, streaming) as u8;
        st.last_error_u16 = StatusCode::Ok as u16;
        st.fps = fps;
        st.frames = frames;
        st.heartbeat = heartbeat;
        publish_status(ptr, st);

        std::thread::sleep(Duration::from_millis(16));
    }

    logger.log(LogLevel::Info, "bridge", "service loop exiting");
    // Stop the stream first so no capture tick touches the video mapping while
    // we restore vtable slots.
    capturer.stop_stream();

    let mut st = StatusSnapshot::ZERO;
    st.generation = heartbeat.wrapping_add(1);
    st.state_u8 = BridgeState::Failed as u8;
    st.heartbeat = heartbeat.wrapping_add(1);
    publish_status(ptr, st);

    if teardown {
        let desc = capturer.uninstall();
        logger.log(LogLevel::Info, "gpu", &format!("hooks uninstalled: {desc}"));
        logger.log(
            LogLevel::Info,
            "bridge",
            "draining in-flight hooks, then unloading the bridge DLL",
        );
        // Let any sim thread currently inside one of our hooks run to completion
        // (each hook just records and tail-calls the original — microseconds) well
        // before the module is freed. Several presents at 60 Hz.
        std::thread::sleep(Duration::from_millis(250));
        // Unloads this DLL and exits the service thread; never returns.
        crate::self_unload();
    }
}

fn state_for(enabled: bool, streaming: bool) -> BridgeState {
    match (enabled, streaming) {
        (false, _) => BridgeState::Ready,
        (true, true) => BridgeState::Running,
        (true, false) => BridgeState::Degraded,
    }
}

fn publish_status(ptr: *mut SharedBlock, status: StatusSnapshot) {
    // SAFETY: single writer (this thread); payload guarded by its seqlock.
    unsafe {
        let payload = core::ptr::addr_of_mut!((*ptr).status.snapshot);
        (*ptr).status.lock.write(payload, status);
    }
}

fn read_config(ptr: *mut SharedBlock) -> Option<infinity_viewports_protocol::ConfigSnapshot> {
    // SAFETY: bounded seqlock read; the host is the single config writer.
    unsafe {
        let payload = core::ptr::addr_of!((*ptr).config.snapshot);
        (*ptr).config.lock.read(payload, 16)
    }
}

fn cstr(buf: &[u8]) -> String {
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Log the render-target / depth-stencil inventory, largest targets first so
/// the full-resolution passes (G-buffer, HDR colour, depth, cloud merge) are at
/// the top. Use it with the layer scan to identify a new viewport's targets.
fn log_inventory(logger: &mut Logger, capturer: &Capturer) {
    let mut inv = capturer.rt_inventory();
    logger.log(
        LogLevel::Info,
        "gpu",
        &format!("RT/DS inventory: {} distinct signatures", inv.len()),
    );
    // Live (actively rendered) targets first, then by area.
    inv.sort_by_key(|e| core::cmp::Reverse((e.live, e.width as u64 * e.height as u64, e.count)));
    for e in inv.iter().take(56) {
        let kind = if e.flags & 0x2 != 0 { "DS" } else { "RT" };
        let live = if e.live { " LIVE" } else { "" };
        logger.log(
            LogLevel::Info,
            "gpu",
            &format!(
                "  {kind}{live} {}x{} fmt={}({}) samp={} x{}",
                e.width,
                e.height,
                e.format,
                dxgi_name(e.format),
                e.samples,
                e.count
            ),
        );
    }
}

/// Short names for the DXGI formats we care about (numbers are DXGI_FORMAT).
fn dxgi_name(f: u32) -> &'static str {
    match f {
        2 => "R32G32B32A32_FLOAT",
        10 => "R16G16B16A16_FLOAT",
        11 => "R16G16B16A16_UNORM",
        24 => "R10G10B10A2_UNORM",
        26 => "R11G11B10_FLOAT",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        34 => "R16G16_FLOAT",
        35 => "R16G16_UNORM",
        40 => "D32_FLOAT",
        41 => "R32_FLOAT",
        44 => "R24G8_TYPELESS",
        45 => "D24_UNORM_S8_UINT",
        54 => "R16_FLOAT",
        55 => "D16_UNORM",
        56 => "R16_UNORM",
        61 => "R8_UNORM",
        63 => "R8_SNORM",
        _ => "?",
    }
}
