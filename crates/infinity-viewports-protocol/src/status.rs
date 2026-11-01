/// Coarse lifecycle of the bridge as the host sees it.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeState {
    Starting = 0,
    /// Handshook; engine contract resolved; idle.
    Ready = 1,
    /// Producing FLIR frames.
    Running = 2,
    /// A feature failed closed; `last_error` says which. HUD unaffected.
    Degraded = 3,
    /// Fatal; the bridge will not drive the view this session.
    Failed = 4,
}

impl BridgeState {
    pub fn from_u8(v: u8) -> BridgeState {
        match v {
            1 => BridgeState::Ready,
            2 => BridgeState::Running,
            3 => BridgeState::Degraded,
            4 => BridgeState::Failed,
            _ => BridgeState::Starting,
        }
    }
}

/// Short error/diagnostic code the bridge reports; the host maps it to text.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCode {
    Ok = 0,
    NoCameraContract = 1,
    GbufferUnmapped = 2,
    CloudRedirectFailed = 3,
    DepthConventionUnknown = 4,
    HudTargetNotFound = 5,
    ViewCreateFailed = 6,
    FenceTimeout = 7,
    ProtocolMismatch = 8,
    WatchdogTripped = 9,
}

impl StatusCode {
    pub fn from_u16(v: u16) -> StatusCode {
        match v {
            1 => StatusCode::NoCameraContract,
            2 => StatusCode::GbufferUnmapped,
            3 => StatusCode::CloudRedirectFailed,
            4 => StatusCode::DepthConventionUnknown,
            5 => StatusCode::HudTargetNotFound,
            6 => StatusCode::ViewCreateFailed,
            7 => StatusCode::FenceTimeout,
            8 => StatusCode::ProtocolMismatch,
            9 => StatusCode::WatchdogTripped,
            _ => StatusCode::Ok,
        }
    }
}

/// The bridge → host status snapshot, published via a seqlock.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct StatusSnapshot {
    pub generation: u64,
    /// Echo of the `ConfigSnapshot.generation` the bridge has applied.
    pub applied_config_generation: u64,
    pub state_u8: u8,
    pub _pad: [u8; 3],
    pub last_error_u16: u16,
    pub _pad2: u16,
    /// Rolling averages.
    pub frame_cost_ms: f32,
    pub view_cost_ms: f32,
    pub fps: f32,
    /// Monotonic frame counter the bridge has produced.
    pub frames: u64,
    /// Bridge heartbeat (monotonic ticks); the host declares the bridge dead if
    /// this stops advancing.
    pub heartbeat: u64,
}

impl StatusSnapshot {
    pub const ZERO: StatusSnapshot = StatusSnapshot {
        generation: 0,
        applied_config_generation: 0,
        state_u8: 0,
        _pad: [0; 3],
        last_error_u16: 0,
        _pad2: 0,
        frame_cost_ms: 0.0,
        view_cost_ms: 0.0,
        fps: 0.0,
        frames: 0,
        heartbeat: 0,
    };
}

/// Severity of a log record.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// One structured log record in the bridge → host log ring. Designed to feed
/// the Manager's `infinity_diag` telemetry.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogRecord {
    /// Monotonic sequence; the host detects drops by gaps.
    pub seq: u64,
    pub level: u8,
    pub _pad: [u8; 3],
    /// Short subsystem tag, NUL-padded (e.g. "gpu", "engine", "scene").
    pub tag: [u8; 16],
    /// NUL-terminated message.
    pub msg: [u8; 220],
}

impl LogRecord {
    pub const EMPTY: LogRecord = LogRecord {
        seq: 0,
        level: 0,
        _pad: [0; 3],
        tag: [0; 16],
        msg: [0; 220],
    };
}

/// Number of log-ring slots.
pub const LOG_RING_SLOTS: usize = 256;
