#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    Lwir = 0,
    Mwir = 1,
}

impl Band {
    pub fn from_u8(v: u8) -> Band {
        match v {
            1 => Band::Mwir,
            _ => Band::Lwir,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    WhiteHot = 0,
    BlackHot = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    PhosphorGreen = 0,
    Gray = 1,
    Ironbow = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HudGeometry {
    pub eye_m: [f32; 3],
    pub fov_y_deg: f32,
    pub aspect: f32,
    pub render_size: [u32; 2],
    pub beacon: BeaconSpec,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BeaconSpec {
    pub xy: [u32; 2],
    pub rgba: [u8; 4],
    pub target_size: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SensorParams {
    pub band_u8: u8,
    pub polarity_u8: u8,
    pub palette_u8: u8,
    pub _pad0: u8,
    pub gain: f32,
    pub level: f32,
    pub gamma: f32,
    pub agc: u8,
    pub detail: u8,
    pub noise: u8,
    pub column_fpn: u8,
    pub agc_clip: f32,
    pub agc_plateau: f32,
    pub detail_gain: f32,
    pub noise_sigma: f32,
    pub agc_speed: f32,
    pub brightness: f32,
}

impl Default for SensorParams {
    fn default() -> Self {
        Self {
            band_u8: Band::Lwir as u8,
            polarity_u8: Polarity::WhiteHot as u8,
            palette_u8: Palette::PhosphorGreen as u8,
            _pad0: 0,
            gain: 1.0,
            level: 0.0,
            gamma: 1.0,
            agc: 1,
            detail: 1,
            noise: 1,
            column_fpn: 1,
            agc_clip: 0.02,
            agc_plateau: 0.5,
            detail_gain: 1.0,
            noise_sigma: 0.004,
            agc_speed: 0.25,
            brightness: 1.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EnvironmentWire {
    pub ambient_temp_c: f32,
    pub pressure_hpa: f32,
    pub humidity_pct: f32,
    pub dewpoint_c: f32,
    pub visibility_m: f32,
    pub precip_kind: u8, // 0 none, 1 rain, 2 snow
    pub in_cloud: u8,
    pub _pad: [u8; 2],
    pub precip_rate: f32,
    pub wind_dir_deg: f32,
    pub wind_speed_ms: f32,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f32,
    pub ground_alt_m: f32,
    pub utc_unix_s: f64,
    pub sun_dir: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ConfigSnapshot {
    pub generation: u64,
    pub enabled: u8,
    pub _pad: [u8; 7],
    pub hud: HudGeometry,
    pub sensor: SensorParams,
    pub env: EnvironmentWire,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandCode {
    Nop = 0,
    DumpCapture = 1,
    Rediscover = 2,
    Shutdown = 3,
    Teardown = 4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CommandSlot {
    pub code: u32,
    pub _pad: u32,
    pub arg: [u8; 240],
}

impl CommandSlot {
    pub const EMPTY: CommandSlot = CommandSlot {
        code: 0,
        _pad: 0,
        arg: [0; 240],
    };
}

pub const COMMAND_RING_SLOTS: usize = 16;
