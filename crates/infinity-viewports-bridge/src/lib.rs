use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::FreeLibraryAndExitThread;

use infinity_viewports_protocol::{BridgeState, StatusCode, Tier};

mod service;
mod shared;

use service::Bridge;

static BRIDGE: OnceLock<Bridge> = OnceLock::new();
/// CAS guard so the `DllMain` bootstrap and an explicit `rtt_bridge_start` call
/// cannot both start the bridge (which would create two mappings and threads).
static STARTING: AtomicBool = AtomicBool::new(false);
/// This DLL's module handle, captured in `DllMain`, so the service thread can
/// unload the bridge cleanly on a teardown command.
static MODULE: AtomicUsize = AtomicUsize::new(0);

/// Unload the bridge DLL and exit the calling (service) thread. Never returns.
/// The caller must have already uninstalled the hooks and drained in-flight
/// hook calls, so no sim thread is executing in this module's code.
pub(crate) fn self_unload() -> ! {
    // FreeLibraryAndExitThread is `-> !`: it drops this module's refcount and
    // exits the calling thread without returning.
    let module = MODULE.load(Ordering::Acquire);
    unsafe { FreeLibraryAndExitThread(HMODULE(module as *mut c_void), 0) }
}

/// Start the bridge once. Idempotent: the first caller wins, later callers are
/// no-ops returning success. Returns 0 on success, nonzero on failure.
fn bootstrap(name: &str) -> u32 {
    if STARTING.swap(true, Ordering::AcqRel) {
        return 0; // already starting/started
    }
    match Bridge::start(name) {
        Ok(bridge) => {
            let _ = BRIDGE.set(bridge);
            0
        }
        Err(code) => code as u32 | 0x8000_0000,
    }
}

/// Explicit start export, for an injector that prefers to call it rather than
/// rely on `DllMain` self-start. `arg`/`len` are an optional NUL-free ASCII name
/// hint; the bridge derives the section name from its own pid regardless.
///
/// # Safety
/// Called by the host via a remote thread with a valid `arg`/`len` (or null/0).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn rtt_bridge_start(arg: *const u8, len: usize) -> u32 {
    let name = if arg.is_null() || len == 0 {
        String::new()
    } else {
        let slice = unsafe { std::slice::from_raw_parts(arg, len) };
        String::from_utf8_lossy(slice).into_owned()
    };
    bootstrap(&name)
}

/// Stop export (optional; the host can also just let the process exit). Leaves
/// hooks installed if any recorded GPU work might still reference bridge
/// resources — a deliberate leak is safer than a crash at teardown.
///
/// # Safety
/// Called by the host.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn rtt_bridge_stop() -> u32 {
    if let Some(b) = BRIDGE.get() {
        b.request_shutdown();
    }
    0
}

/// Standard `DllMain`. On `DLL_PROCESS_ATTACH` (reason 1) we self-bootstrap by
/// spawning the service thread, so plain `LoadLibraryW` injection is enough —
/// the host needs no second remote thread, side-stepping the x64 problem that a
/// remote thread's 32-bit exit code cannot carry a 64-bit module base. We do the
/// minimum under loader lock: spawn one thread and return; all real work runs on
/// that thread after the lock releases.
///
/// # Safety
/// Called by the loader.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    module: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module as usize, Ordering::Release);
        // Spawn the bootstrap off the loader-lock thread.
        let _ = std::thread::Builder::new()
            .name("rtt-bootstrap".into())
            .spawn(|| {
                bootstrap("");
            });
    }
    1
}

/// Re-exported for tests and `rtt-testhost` so they can assert the tier logic
/// without the sim.
pub fn describe_tier(t: Tier) -> &'static str {
    match t {
        Tier::Off => "off",
        Tier::Full => "full (g-buffer + depth + clouds)",
        Tier::DepthOnly => "depth-only",
        Tier::ColourOnly => "colour-only",
    }
}

pub fn describe_state(s: BridgeState) -> &'static str {
    match s {
        BridgeState::Starting => "starting",
        BridgeState::Ready => "ready",
        BridgeState::Running => "running",
        BridgeState::Degraded => "degraded",
        BridgeState::Failed => "failed",
    }
}

pub fn describe_error(c: StatusCode) -> &'static str {
    match c {
        StatusCode::Ok => "ok",
        StatusCode::NoCameraContract => "no camera contract",
        StatusCode::GbufferUnmapped => "g-buffer layout not mapped",
        StatusCode::CloudRedirectFailed => "cloud-merge redirect failed",
        StatusCode::DepthConventionUnknown => "depth convention not determined",
        StatusCode::HudTargetNotFound => "HUD render target not found",
        StatusCode::ViewCreateFailed => "FLIR view creation failed",
        StatusCode::FenceTimeout => "GPU fence timeout",
        StatusCode::ProtocolMismatch => "protocol version mismatch",
        StatusCode::WatchdogTripped => "presentation watchdog tripped",
    }
}
