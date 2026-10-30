use std::fmt;

pub type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug)]
pub enum HostError {
    SimNotFound,
    BridgeImage(String),
    Inject(String),
    Handshake(String),
    ProtocolMismatch { bridge_major: u32, host_major: u32 },
    NotConnected,
    Timeout,
    Os(String),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::SimNotFound => write!(f, "Simulator not found"),
            HostError::BridgeImage(msg) => write!(f, "Bridge image error: {}", msg),
            HostError::Inject(msg) => write!(f, "Inject error: {}", msg),
            HostError::Handshake(msg) => write!(f, "Handshake error: {}", msg),
            HostError::ProtocolMismatch {
                bridge_major,
                host_major,
            } => write!(
                f,
                "Protocol mismatch: bridge_major={}, host_major={}",
                bridge_major, host_major
            ),
            HostError::NotConnected => write!(f, "Not connected"),
            HostError::Timeout => write!(f, "Timeout"),
            HostError::Os(msg) => write!(f, "OS error: {}", msg),
        }
    }
}

impl std::error::Error for HostError {}
