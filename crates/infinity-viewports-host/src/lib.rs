pub mod camera;
pub mod channel;
pub mod error;
pub mod geo;
pub mod inject;
pub mod locate;
pub mod simvars;
pub mod video;

use std::path::PathBuf;
use std::sync::Arc;

pub use error::{HostError, Result};
pub use infinity_viewports_protocol::{BridgeState, StatusCode};

use channel::Channel;

#[derive(Clone, Debug, Default)]
pub enum SimTarget {
    #[default]
    Auto,
    Pid(u32),
    Path(PathBuf),
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub bridge_path: PathBuf,
    pub bridge_sha256: Option<String>,
    pub sim_path: Option<PathBuf>,
}

pub struct Host {
    config: Arc<HostConfig>,
}

impl Host {
    pub fn new(config: HostConfig) -> Result<Host> {
        Ok(Host {
            config: Arc::new(config),
        })
    }

    pub fn attach(&self, target: SimTarget) -> Result<Sim> {
        let proc = match target {
            SimTarget::Auto => locate::select(self.config.sim_path.as_deref())?,
            SimTarget::Pid(pid) => locate::SimProcess {
                pid,
                exe_path: None,
            },
            SimTarget::Path(ref p) => locate::select(Some(p))?,
        };
        inject::verify_bridge(
            &self.config.bridge_path,
            self.config.bridge_sha256.as_deref(),
        )?;
        let injected = inject::inject(proc.pid, &self.config.bridge_path)?;
        let channel = open_with_retry(proc.pid, std::time::Duration::from_secs(5))?;
        channel.verify_protocol()?;
        Ok(Sim {
            pid: proc.pid,
            channel: Arc::new(channel),
            _injected: Some(injected),
        })
    }

    pub fn connect(&self, target: SimTarget) -> Result<Sim> {
        let proc = match target {
            SimTarget::Auto => locate::select(self.config.sim_path.as_deref())?,
            SimTarget::Pid(pid) => locate::SimProcess {
                pid,
                exe_path: None,
            },
            SimTarget::Path(ref p) => locate::select(Some(p))?,
        };
        let channel = open_with_retry(proc.pid, std::time::Duration::from_secs(5))?;
        channel.verify_protocol()?;
        Ok(Sim {
            pid: proc.pid,
            channel: Arc::new(channel),
            _injected: None,
        })
    }
}

fn open_with_retry(pid: u32, budget: std::time::Duration) -> Result<Channel> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match Channel::open(pid) {
            Ok(c) => return Ok(c),
            Err(HostError::NotConnected) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

pub struct Sim {
    pid: u32,
    channel: Arc<Channel>,
    _injected: Option<inject::Injected>,
}

impl Sim {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Bridge on/off. Off leaves the hooks installed but idle.
    pub fn enable(&self, on: bool) -> Result<()> {
        self.channel.update_config(|c| c.enabled = on as u8)
    }

    pub fn camera(&self) -> Result<camera::CameraLink> {
        camera::CameraLink::open(self.pid)
    }

    pub fn status(&self) -> Result<Status> {
        self.channel.read_status().map(Status::from_wire)
    }

    pub fn dump_capture(&self, path: &str) -> Result<()> {
        self.channel.dump_capture(path)
    }

    pub fn start_stream(&self) -> Result<()> {
        self.channel.dump_capture("stream:start")
    }

    pub fn stop_stream(&self) -> Result<()> {
        self.channel.dump_capture("stream:stop")
    }

    pub fn set_stream_source(&self, format: u32, occurrence: u32) -> Result<()> {
        self.channel
            .dump_capture(&format!("stream:src:{format}:{occurrence}"))
    }

    pub fn open_video(&self) -> Result<video::VideoReader> {
        video::VideoReader::open(self.pid)
    }

    pub fn teardown_bridge(&self) -> Result<()> {
        self.channel
            .send_command(infinity_viewports_protocol::CommandCode::Teardown, &[])
    }

    pub fn teardown_nose(&self) -> Result<()> {
        signal_nose_teardown(self.pid)
    }

    pub fn drain_logs(&self) -> Vec<LogLine> {
        let mut buf = [infinity_viewports_protocol::LogRecord::EMPTY; 64];
        let n = self.channel.drain_logs(&mut buf);
        buf[..n].iter().map(LogLine::from_wire).collect()
    }
}

fn signal_nose_teardown(pid: u32) -> Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent};
    use windows::core::PCWSTR;

    const EVENT_MODIFY_STATE: u32 = 0x0002;
    let name: Vec<u16> = format!("Local\\InfinityViewports.{pid}.teardown\0")
        .encode_utf16()
        .collect();
    unsafe {
        let handle = OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE),
            false,
            PCWSTR(name.as_ptr()),
        )
        .map_err(|_| HostError::NotConnected)?;
        let r = SetEvent(handle);
        let _ = CloseHandle(handle);
        r.map_err(|e| HostError::Os(format!("SetEvent: {e}")))?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub seq: u64,
    pub level: u8,
    pub tag: String,
    pub msg: String,
}

impl LogLine {
    fn from_wire(r: &infinity_viewports_protocol::LogRecord) -> LogLine {
        let cut = |b: &[u8]| {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..n]).into_owned()
        };
        LogLine {
            seq: r.seq,
            level: r.level,
            tag: cut(&r.tag),
            msg: cut(&r.msg),
        }
    }
    pub fn level_str(&self) -> &'static str {
        match self.level {
            0 => "TRACE",
            1 => "DEBUG",
            2 => "INFO",
            3 => "WARN",
            _ => "ERROR",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: BridgeState,
    pub last_error: StatusCode,
    pub frame_cost_ms: f32,
    pub view_cost_ms: f32,
    pub fps: f32,
    pub frames: u64,
    pub heartbeat: u64,
}

impl Status {
    fn from_wire(s: infinity_viewports_protocol::StatusSnapshot) -> Status {
        Status {
            state: BridgeState::from_u8(s.state_u8),
            last_error: StatusCode::from_u16(s.last_error_u16),
            frame_cost_ms: s.frame_cost_ms,
            view_cost_ms: s.view_cost_ms,
            fps: s.fps,
            frames: s.frames,
            heartbeat: s.heartbeat,
        }
    }
}
