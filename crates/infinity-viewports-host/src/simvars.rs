/**
* Shim to read a small list of simvars without bringing in the full infinity-rs SimConnect bindings
*/
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use infinity_viewports_protocol::AircraftPose;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, PCWSTR};

use crate::geo::aircraft_pose;

const FEET_TO_M: f64 = 0.3048;
const STALE: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(2);
const E_FAIL: i32 = 0x80004005u32 as i32;
const FLOAT64: i32 = 4;
const PERIOD_SIM_FRAME: i32 = 3;
const DATA_OFFSET: usize = 40;

const DATUMS: &[(&str, &str)] = &[
    ("PLANE LATITUDE", "degrees"),
    ("PLANE LONGITUDE", "degrees"),
    ("PLANE ALTITUDE", "feet"),
    ("PLANE PITCH DEGREES", "degrees"),
    ("PLANE BANK DEGREES", "degrees"),
    ("PLANE HEADING DEGREES TRUE", "degrees"),
];

type ScOpen = unsafe extern "system" fn(
    *mut *mut c_void,
    *const u8,
    *mut c_void,
    u32,
    *mut c_void,
    u32,
) -> i32;
type ScClose = unsafe extern "system" fn(*mut c_void) -> i32;
type ScAdd =
    unsafe extern "system" fn(*mut c_void, u32, *const u8, *const u8, i32, f32, u32) -> i32;
type ScRequest =
    unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32, u32, u32, u32) -> i32;
type ScNext = unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut u32) -> i32;

struct Api {
    open: ScOpen,
    close: ScClose,
    add: ScAdd,
    request: ScRequest,
    next: ScNext,
}

static API: OnceLock<Api> = OnceLock::new();

/// One SimConnect reading, RAW: the sim's own units and signs
/// (`PLANE PITCH DEGREES` / `PLANE BANK DEGREES` as MSFS reports them).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_ft: f64,
    pub pitch_deg: f64,
    pub bank_deg: f64,
    pub heading_deg: f64,
}

struct Conn {
    handle: *mut c_void,
    exception: Option<u32>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(api) = API.get() {
            if !self.handle.is_null() {
                unsafe { (api.close)(self.handle) };
            }
        }
    }
}

pub struct AircraftFeed {
    conn: Option<Conn>,
    sample: Option<Sample>,
    seen: Option<Instant>,
    next_try: Instant,
    error: Option<String>,
    exception: Option<u32>,
}

impl AircraftFeed {
    pub fn new() -> Self {
        Self {
            conn: None,
            sample: None,
            seen: None,
            next_try: Instant::now(),
            error: None,
            exception: None,
        }
    }

    pub fn poll(&mut self) {
        if self.conn.is_none() {
            if Instant::now() < self.next_try {
                return;
            }
            match Conn::open() {
                Ok(conn) => {
                    self.conn = Some(conn);
                    self.error = None;
                }
                Err(err) => {
                    self.error = Some(err);
                    self.next_try = Instant::now() + RETRY;
                }
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        match conn.drain() {
            Ok(sample) => {
                self.exception = conn.exception;
                if let Some(sample) = sample {
                    self.sample = Some(sample);
                    self.seen = Some(Instant::now());
                }
            }
            Err(err) => {
                self.error = Some(err);
                self.conn = None;
                self.next_try = Instant::now() + RETRY;
            }
        }
    }

    /// The latest reading while it is fresh (under a second old), for a
    /// consumer that builds its own pose with [`crate::geo::aircraft_pose`]
    /// — e.g. to correct a sign convention without rebuilding this crate.
    pub fn sample(&self) -> Option<Sample> {
        let (sample, seen) = (self.sample.as_ref()?, self.seen?);
        (seen.elapsed() <= STALE).then_some(*sample)
    }

    /// Why the feed has no data, if it has none (SimConnect missing, sim
    /// closed, ...).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn pose(&self) -> AircraftPose {
        let (Some(sample), Some(seen)) = (&self.sample, self.seen) else {
            return AircraftPose::absent();
        };
        if seen.elapsed() > STALE {
            return AircraftPose::absent();
        }
        aircraft_pose(
            sample.lat_deg,
            sample.lon_deg,
            sample.alt_ft * FEET_TO_M,
            sample.heading_deg,
            sample.pitch_deg,
            sample.bank_deg,
        )
    }

    pub fn line(&self) -> String {
        if let Some(sample) = &self.sample {
            let stale = self.seen.is_none_or(|t| t.elapsed() > STALE);
            let mark = if stale { " stale" } else { "" };
            let exc = self
                .exception
                .map(|c| format!(" exception {c}"))
                .unwrap_or_default();
            let ns = if sample.lat_deg >= 0.0 { "N" } else { "S" };
            let ew = if sample.lon_deg >= 0.0 { "E" } else { "W" };
            return format!(
                "SimConnect: {:.3} deg {ns} {:.3} deg {ew}  {:.0} m  hdg {:.0} deg p {:.1} deg  b {:.1}{mark}{exc}",
                sample.lat_deg.abs(),
                sample.lon_deg.abs(),
                sample.alt_ft * FEET_TO_M,
                sample.heading_deg,
                sample.pitch_deg,
                sample.bank_deg,
            );
        }
        if let Some(err) = &self.error {
            return format!("SimConnect: {err}");
        }
        "SimConnect: waiting for the sim".into()
    }
}

impl Default for AircraftFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl Conn {
    fn open() -> Result<Self, String> {
        let api = api()?;
        let mut handle = ptr::null_mut();
        let name = b"Infinity Viewports\0";
        let hr = unsafe {
            (api.open)(
                &mut handle,
                name.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                0,
            )
        };
        if hr < 0 || handle.is_null() {
            return Err(format!("waiting for the sim({hr:#x}"));
        }

        for (i, (datum, unit)) in DATUMS.iter().enumerate() {
            let datum = c_string(datum);
            let unit = c_string(unit);
            let hr = unsafe {
                (api.add)(
                    handle,
                    1,
                    datum.as_ptr(),
                    unit.as_ptr(),
                    FLOAT64,
                    0.0,
                    u32::MAX,
                )
            };
            if hr < 0 {
                unsafe { (api.close)(handle) };
                return Err(format!("define {} failed {hr:#x}", i));
            }
        }
        let hr = unsafe { (api.request)(handle, 1, 1, 0, PERIOD_SIM_FRAME, 0, 0, 0, 0) };
        if hr < 0 {
            unsafe { (api.close)(handle) };
            return Err(format!("request failed {hr:#x}"));
        }
        Ok(Conn {
            handle,
            exception: None,
        })
    }

    fn drain(&mut self) -> Result<Option<Sample>, String> {
        let Some(api) = API.get() else {
            return Err("SimConnect not loaded".into());
        };
        let mut last = None;
        for _ in 0..64 {
            let mut data = ptr::null_mut();
            let mut size = 0u32;
            let hr = unsafe { (api.next)(self.handle, &mut data, &mut size) };
            if hr < 0 {
                if hr == E_FAIL {
                    return Ok(last);
                }
                return Err(format!("read failed {hr:#x}"));
            }
            if data.is_null() || size < 12 {
                continue;
            }
            let bytes = data as *const u8;
            let id = read_u32(bytes, 8);
            match id {
                3 => return Err("sim closed the SimConnect session".into()),
                1 => {
                    if size >= 16 {
                        self.exception = Some(read_u32(bytes, 12));
                    }
                }
                8 if size >= (DATA_OFFSET + 48) as u32 && read_u32(bytes, 12) == 1 => {
                    last = Some(Sample {
                        lat_deg: read_f64(bytes, DATA_OFFSET),
                        lon_deg: read_f64(bytes, DATA_OFFSET + 8),
                        alt_ft: read_f64(bytes, DATA_OFFSET + 16),
                        pitch_deg: read_f64(bytes, DATA_OFFSET + 24),
                        bank_deg: read_f64(bytes, DATA_OFFSET + 32),
                        heading_deg: read_f64(bytes, DATA_OFFSET + 40),
                    });
                }
                _ => {}
            }
        }
        Ok(last)
    }
}

fn api() -> Result<&'static Api, String> {
    if let Some(api) = API.get() {
        return Ok(api);
    }
    let loaded = load_api()?;
    let _ = API.set(loaded);
    API.get().ok_or_else(|| "SimConnect init failed".into())
}

fn load_api() -> Result<Api, String> {
    let module = load_module()?;
    let api = Api {
        open: proc(module, "SimConnect_Open")?,
        close: proc(module, "SimConnect_Close")?,
        add: proc(module, "SimConnect_AddToDataDefinition")?,
        request: proc(module, "SimConnect_RequestDataOnSimObject")?,
        next: proc(module, "SimConnect_GetNextDispatch")?,
    };
    let _ = module;
    Ok(api)
}

fn load_module() -> Result<windows::Win32::Foundation::HMODULE, String> {
    let mut tried = Vec::new();
    if let Ok(root) = std::env::var("MSFS_SDK") {
        tried.push(Path::new(&root).join(r"SimConnect SDK\lib\SimConnect.dll"));
    }
    tried.push(Path::new(r"C:\MSFS 2024 SDK\SimConnect SDK\lib\SimConnect.dll").to_path_buf());
    tried.push(Path::new(r"C:\MSFS SDK\SimConnect SDK\lib\SimConnect.dll").to_path_buf());
    for path in &tried {
        if path.is_file() {
            if let Some(module) = load_library(path) {
                return Ok(module);
            }
        }
    }
    if let Some(module) = load_library(Path::new("SimConnect.dll")) {
        return Ok(module);
    }
    Err("SimConnect.dll not found".into())
}

fn load_library(path: &Path) -> Option<windows::Win32::Foundation::HMODULE> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())).ok() }
}

fn proc<T: Copy>(module: windows::Win32::Foundation::HMODULE, name: &str) -> Result<T, String> {
    assert_eq!(size_of::<T>(), size_of::<usize>());
    let c = c_string(name);
    let far = unsafe { GetProcAddress(module, PCSTR(c.as_ptr())) }
        .ok_or_else(|| format!("SimConnect.dll is missing {name}"))?;
    Ok(unsafe { std::mem::transmute_copy(&far) })
}

fn c_string(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn read_u32(p: *const u8, off: usize) -> u32 {
    let mut buf = [0u8; 4];
    unsafe { ptr::copy_nonoverlapping(p.add(off), buf.as_mut_ptr(), 4) };
    u32::from_le_bytes(buf)
}

fn read_f64(p: *const u8, off: usize) -> f64 {
    let mut buf = [0u8; 8];
    unsafe { ptr::copy_nonoverlapping(p.add(off), buf.as_mut_ptr(), 8) };
    f64::from_le_bytes(buf)
}
