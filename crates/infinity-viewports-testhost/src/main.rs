use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use infinity_viewports_host::{Host, HostConfig, SimTarget};
use infinity_viewports_protocol::{EnvironmentWire, HudGeometry, SensorParams};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut bridge: Option<PathBuf> = None;
    let mut dump: Option<String> = None;
    let mut once = false;
    let mut watch_stream: Option<String> = None;
    let mut stream_color: Option<u32> = None;
    let mut stream_occ: u32 = 0;
    let mut teardown = false;
    let mut peek = false;
    let mut grab: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bridge" => bridge = args.next().map(PathBuf::from),
            // Log the bridge's RT/DS inventory (identify a viewport's targets).
            "--inventory" => dump = Some("inventory".to_string()),
            // Live stream: start it and save the latest frame as an RTGB file
            // every second (render with scratchpad/render_rtgb.py).
            "--watch-stream" => watch_stream = args.next(),
            "--stream-color" => stream_color = args.next().and_then(|s| s.parse().ok()),
            "--stream-occ" => stream_occ = args.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            // Scan EVERY readable 1024x576 RT in one frame (identify colour/normals/material).
            "--scan-nose-layers" => dump = args.next().map(|p| format!("scan:nose:{p}")),
            // Remote teardown: unhook + unload both DLLs, leaving the sim running.
            "--teardown" => teardown = true,
            // Read the resident bridge log. Does not inject or unload.
            "--peek" => peek = true,
            // Save a few streamed colour occurrences as BMP. Does not inject.
            "--grab" => grab = args.next(),
            "--once" => once = true,
            _ => {}
        }
    }
    let Some(bridge_path) = bridge else {
        eprintln!(
            "usage: rtt-testhost --bridge <bridge.dll> [--once] [--inventory] [--scan-nose-layers <name>] [--watch-stream <out>] [--teardown|--peek|--grab <dir>]"
        );
        return ExitCode::from(2);
    };

    let host = match Host::new(HostConfig {
        bridge_path,
        bridge_sha256: None,
        sim_path: None,
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("host init: {e}");
            return ExitCode::FAILURE;
        }
    };

    if teardown {
        let sim = match host.connect(SimTarget::Auto) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("connect: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("connected to pid {} (no inject)", sim.pid());
        match sim.teardown_nose() {
            Ok(()) => println!("nose teardown signalled"),
            Err(e) => eprintln!("nose teardown: {e} (rtt_nose.dll not loaded?)"),
        }
        match sim.teardown_bridge() {
            Ok(()) => println!("bridge teardown command sent"),
            Err(e) => eprintln!("bridge teardown: {e}"),
        }
        for _ in 0..20 {
            for line in sim.drain_logs() {
                println!("  [{}] {}: {}", line.level_str(), line.tag, line.msg);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        return ExitCode::SUCCESS;
    }

    if peek {
        let sim = match host.connect(SimTarget::Auto) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("connect: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("peek pid {} (no inject)", sim.pid());
        for _ in 0..8 {
            for line in sim.drain_logs() {
                println!("  [{}] {}: {}", line.level_str(), line.tag, line.msg);
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
        return ExitCode::SUCCESS;
    }

    if let Some(dir) = grab {
        let sim = match host.connect(SimTarget::Auto) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("connect: {e}");
                return ExitCode::FAILURE;
            }
        };
        std::fs::create_dir_all(&dir).ok();
        let mut reader = match sim.open_video() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("video: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("grab pid {} -> {dir}", sim.pid());
        for occ in 0..6u32 {
            if let Err(e) = sim.set_stream_source(26, occ) {
                eprintln!("src {occ}: {e}");
                continue;
            }
            std::thread::sleep(Duration::from_millis(250));
            let mut frame = None;
            for _ in 0..20 {
                if let Some(f) = reader.read_new() {
                    frame = Some(f);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let Some(frame) = frame else {
                println!("occ {occ}: no frame");
                continue;
            };
            let Some(color) = frame.layers.iter().find(|l| l.role == 2) else {
                println!("occ {occ}: seq {} has no colour layer", frame.seq);
                continue;
            };
            let path = format!("{dir}\\occ{occ}.bmp");
            match write_bmp(&path, color) {
                Ok(stats) => println!(
                    "occ {occ}: seq {} fmt {} {}x{} {}",
                    frame.seq, color.format, color.width, color.height, stats
                ),
                Err(e) => println!("occ {occ}: {e}"),
            }
        }
        let _ = sim.set_stream_source(26, 0);
        println!("restored stream colour to fmt 26 occ 0");
        return ExitCode::SUCCESS;
    }

    let sim = match host.attach(SimTarget::Auto) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("attach: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("attached to pid {}", sim.pid());

    let flir = sim.flir();
    let _ = flir.configure(HudGeometry {
        eye_m: [0.0, 0.1, 0.3],
        fov_y_deg: 24.0,
        aspect: 1.33,
        render_size: [1024, 768],
        beacon: infinity_viewports_protocol::BeaconSpec {
            xy: [0, 0],
            rgba: [1, 2, 3, 255],
            target_size: [1024, 768],
        },
    });
    let _ = flir.set_sensor(SensorParams::default());
    let _ = flir.set_environment(fake_env());
    let _ = flir.enable(true);

    if let Some(out) = &watch_stream {
        if let Some(fmt) = stream_color {
            let _ = sim.set_stream_source(fmt, stream_occ);
            println!("stream colour source -> fmt {fmt} occ {stream_occ}");
        }
        match sim.start_stream() {
            Ok(()) => println!("stream start requested"),
            Err(e) => eprintln!("stream start: {e}"),
        }
        let mut reader: Option<infinity_viewports_host::video::VideoReader> = None;
        let mut saved = 0u64;
        loop {
            for line in sim.drain_logs() {
                println!("  [{}] {}: {}", line.level_str(), line.tag, line.msg);
            }
            if reader.is_none() {
                reader = sim.open_video().ok();
            }
            if let Some(r) = &mut reader {
                if let Some(frame) = r.read_new() {
                    match write_rtgb(out, &frame) {
                        Ok(bytes) => {
                            saved += 1;
                            println!(
                                "frame seq={} ({} layers, {} bytes) -> {out}",
                                frame.seq,
                                frame.layers.len(),
                                bytes
                            );
                        }
                        Err(e) => eprintln!("write {out}: {e}"),
                    }
                }
            }
            if let Ok(s) = sim.status() {
                println!(
                    "  [stream] saved={saved} state={:?} hb={}",
                    s.state, s.heartbeat
                );
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
    }

    if let Some(cmd) = &dump {
        match sim.dump_capture(cmd) {
            Ok(()) => println!("sent capture command {cmd:?}"),
            Err(e) => eprintln!("capture command: {e}"),
        }
    }

    loop {
        for line in sim.drain_logs() {
            println!("  [{}] {}: {}", line.level_str(), line.tag, line.msg);
        }
        match sim.status() {
            Ok(s) => println!(
                "state={:?} tier={:?} err={:?} hb={} fps={:.1} frames={} view={:.2}ms",
                s.state, s.tier, s.last_error, s.heartbeat, s.fps, s.frames, s.view_cost_ms
            ),
            Err(e) => println!("status: {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    ExitCode::SUCCESS
}

fn write_rtgb(
    path: &str,
    frame: &infinity_viewports_host::video::VideoFrame,
) -> std::io::Result<usize> {
    let mut out = Vec::new();
    out.extend_from_slice(b"RTGB");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(frame.layers.len() as u32).to_le_bytes());
    for l in &frame.layers {
        out.extend_from_slice(&l.role.to_le_bytes());
        out.extend_from_slice(&l.format.to_le_bytes());
        out.extend_from_slice(&l.width.to_le_bytes());
        out.extend_from_slice(&l.height.to_le_bytes());
        out.extend_from_slice(&(l.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&l.bytes);
    }
    std::fs::write(path, &out)?;
    Ok(out.len())
}

fn write_bmp(
    path: &str,
    layer: &infinity_viewports_host::video::VideoLayerData,
) -> Result<String, String> {
    let w = layer.width as usize;
    let h = layer.height as usize;
    if layer.format != 26 || w == 0 || h == 0 {
        return Err(format!(
            "fmt {} {}x{}",
            layer.format, layer.width, layer.height
        ));
    }
    let src_row = w * 4;
    if layer.bytes.len() < src_row * h {
        return Err(format!("short buffer {}", layer.bytes.len()));
    }
    let mut white = 0u32;
    let mut sky_n = 0u32;
    let mut sky_sum = 0f32;
    let sky_rows = (h / 3).max(1);
    let dst_row = (w * 3 + 3) & !3;
    let mut pixels = vec![0u8; dst_row * h];
    for y in 0..h {
        for x in 0..w {
            let o = y * src_row + x * 4;
            let p = u32::from_le_bytes([
                layer.bytes[o],
                layer.bytes[o + 1],
                layer.bytes[o + 2],
                layer.bytes[o + 3],
            ]);
            let r = f11(p & 0x7ff);
            let g = f11((p >> 11) & 0x7ff);
            let b = f10(((p >> 22) & 0x3ff) as u16);
            let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            if y < sky_rows {
                sky_n += 1;
                sky_sum += lum;
                let mx = r.max(g).max(b);
                let mn = r.min(g).min(b);
                if lum > 0.15 && mx > 0.0 && mn > mx * 0.65 {
                    white += 1;
                }
            }
            let tm = |c: f32| {
                let v = (c / (1.0 + c)).clamp(0.0, 1.0);
                (v.powf(1.0 / 2.2) * 255.0) as u8
            };
            let dy = h - 1 - y;
            let i = dy * dst_row + x * 3;
            pixels[i] = tm(b);
            pixels[i + 1] = tm(g);
            pixels[i + 2] = tm(r);
        }
    }
    let image_size = (dst_row * h) as u32;
    let file_size = 54 + image_size;
    let mut file = Vec::with_capacity(file_size as usize);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&file_size.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&(w as i32).to_le_bytes());
    file.extend_from_slice(&(h as i32).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&24u16.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&image_size.to_le_bytes());
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&pixels);
    std::fs::write(path, &file).map_err(|e| e.to_string())?;
    let mean = sky_sum / sky_n as f32;
    let frac = white as f32 / sky_n as f32;
    Ok(format!(
        "upper-third mean {mean:.3} near-white {:.1}%",
        frac * 100.0
    ))
}

fn f11(u: u32) -> f32 {
    let e = (u >> 6) & 0x1f;
    let m = u & 0x3f;
    if e == 0 {
        m as f32 / 64.0 * 2f32.powi(-14)
    } else {
        (1.0 + m as f32 / 64.0) * 2f32.powi(e as i32 - 15)
    }
}

fn f10(u: u16) -> f32 {
    let e = ((u >> 5) & 0x1f) as u32;
    let m = (u & 0x1f) as u32;
    if e == 0 {
        m as f32 / 32.0 * 2f32.powi(-14)
    } else {
        (1.0 + m as f32 / 32.0) * 2f32.powi(e as i32 - 15)
    }
}

fn fake_env() -> EnvironmentWire {
    EnvironmentWire {
        ambient_temp_c: 15.0,
        pressure_hpa: 1013.0,
        humidity_pct: f32::NAN,
        dewpoint_c: 9.0,
        visibility_m: 15_000.0,
        precip_kind: 0,
        in_cloud: 0,
        _pad: [0; 2],
        precip_rate: 0.0,
        wind_dir_deg: 270.0,
        wind_speed_ms: 5.0,
        lat_deg: 47.46,
        lon_deg: -122.3,
        alt_m: 500.0,
        ground_alt_m: 130.0,
        utc_unix_s: 1_718_971_200.0,
        sun_dir: [f32::NAN; 3],
    }
}
