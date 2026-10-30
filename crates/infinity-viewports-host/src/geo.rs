use infinity_viewports_protocol::AircraftPose;

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257223563;
const WGS84_E2: f64 = WGS84_F * (2.0 - WGS84_F);

pub fn aircraft_pose(
    lat_deg: f64,
    lon_deg: f64,
    alt_m: f64,
    heading_deg: f64,
    pitch_deg: f64,
    bank_deg: f64,
) -> AircraftPose {
    let Some(origin) = ecef(lat_deg, lon_deg, alt_m) else {
        return AircraftPose::absent();
    };
    let Some((right, up, forward)) = body(lat_deg, lon_deg, heading_deg, pitch_deg, bank_deg)
    else {
        return AircraftPose::absent();
    };
    AircraftPose {
        valid: 1,
        _pad: 0,
        origin,
        right,
        up,
        forward,
    }
}

fn ecef(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Option<[f64; 3]> {
    if !(-90.0..=90.0).contains(&lat_deg) || !(-2_000.0..=100_000.0).contains(&alt_m) {
        return None;
    }
    if !lat_deg.is_finite() || !lon_deg.is_finite() || !alt_m.is_finite() {
        return None;
    }
    let lon = wrap_lon(lon_deg);
    let lat = lat_deg.to_radians();
    let lon = lon.to_radians();
    let (slat, clat) = (lat.sin(), lat.cos());
    let (slon, clon) = (lon.sin(), lon.cos());
    let n = WGS84_A / (1.0 - WGS84_E2 * slat * slat).sqrt();
    Some([
        (n + alt_m) * clat * clon,
        (n + alt_m) * clat * slon,
        (n * (1.0 - WGS84_E2) + alt_m) * slat,
    ])
}

fn body(
    lat_deg: f64,
    lon_deg: f64,
    heading_deg: f64,
    pitch_deg: f64,
    bank_deg: f64,
) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    if !heading_deg.is_finite() || !pitch_deg.is_finite() || !bank_deg.is_finite() {
        return None;
    }
    let (east, north, up) = enu(lat_deg, lon_deg);
    let h = heading_deg.to_radians();
    let (sh, ch) = (h.sin(), h.cos());
    let mut forward = add(scale(north, ch), scale(east, sh));
    let mut right = add(scale(east, ch), scale(north, -sh));
    let mut up_b = up;

    let p = pitch_deg.to_radians();
    let (sp, cp) = (p.sin(), p.cos());
    let pitched = add(scale(forward, cp), scale(up_b, sp));
    up_b = add(scale(up_b, cp), scale(forward, -sp));
    forward = pitched;

    let b = bank_deg.to_radians();
    let (sb, cb) = (b.sin(), b.cos());
    let banked = add(scale(right, cb), scale(up_b, -sb));
    up_b = add(scale(up_b, cb), scale(right, sb));
    right = banked;

    let right = normalize(right)?;
    let up_b = normalize(up_b)?;
    let forward = normalize(forward)?;
    let swung = cross(forward, up_b);
    if dist2(swung, right) > 1e-6 {
        return None;
    }
    Some((right, up_b, forward))
}

fn enu(lat_deg: f64, lon_deg: f64) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let lat = lat_deg.to_radians();
    let lon = wrap_lon(lon_deg).to_radians();
    let (slat, clat) = (lat.sin(), lat.cos());
    let (slon, clon) = (lon.sin(), lon.cos());
    let east = [-slon, clon, 0.0];
    let north = [-slat * clon, -slat * slon, clat];
    let up = [clat * clon, clat * slon, slat];
    (east, north, up)
}

fn wrap_lon(lon: f64) -> f64 {
    let mut x = lon % 360.0;
    if x > 180.0 {
        x -= 360.0;
    }
    if x <= -180.0 {
        x += 360.0;
    }
    x
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f64; 3]) -> Option<[f64; 3]> {
    let n = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if !n.is_finite() || n < 1e-12 {
        return None;
    }
    Some(scale(v, 1.0 / n.sqrt()))
}

fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    d0 * d0 + d1 * d1 + d2 * d2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3], tol: f64) {
        for i in 0..3 {
            assert!(
                (a[i] - b[i]).abs() < tol,
                "axis {i}: {} vs {} (tol {tol})",
                a[i],
                b[i]
            );
        }
    }

    #[test]
    fn vienna_ecef_matches_a_published_wgs84_point() {
        let pose = aircraft_pose(
            48.14928606302,
            16.28383433228,
            286.40506914692,
            0.0,
            0.0,
            0.0,
        );
        assert_eq!(pose.valid, 1);
        close(
            pose.origin,
            [4_092_523.6549, 1_195_484.3519, 4_728_180.7406],
            0.05,
        );
    }

    #[test]
    fn level_north_at_the_equator_on_the_prime_meridian() {
        let pose = aircraft_pose(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        close(pose.origin, [WGS84_A, 0.0, 0.0], 1e-6);
        close(pose.forward, [0.0, 0.0, 1.0], 1e-9);
        close(pose.right, [0.0, 1.0, 0.0], 1e-9);
        close(pose.up, [1.0, 0.0, 0.0], 1e-9);
    }

    #[test]
    fn heading_east_points_the_nose_east_and_the_right_wing_south() {
        let pose = aircraft_pose(0.0, 0.0, 0.0, 90.0, 0.0, 0.0);
        close(pose.forward, [0.0, 1.0, 0.0], 1e-9);
        close(pose.right, [0.0, 0.0, -1.0], 1e-9);
        close(pose.up, [1.0, 0.0, 0.0], 1e-9);
    }

    #[test]
    fn bank_right_drops_the_right_wing() {
        let pose = aircraft_pose(0.0, 0.0, 0.0, 0.0, 0.0, 90.0);
        close(pose.forward, [0.0, 0.0, 1.0], 1e-9);
        close(pose.right, [-1.0, 0.0, 0.0], 1e-9);
        close(pose.up, [0.0, 1.0, 0.0], 1e-9);
    }

    #[test]
    fn pitch_up_ninety_points_the_nose_at_the_sky() {
        let pose = aircraft_pose(0.0, 0.0, 0.0, 0.0, 90.0, 0.0);
        close(pose.forward, [1.0, 0.0, 0.0], 1e-9);
        close(pose.up, [0.0, 0.0, -1.0], 1e-9);
        close(pose.right, [0.0, 1.0, 0.0], 1e-9);
    }

    #[test]
    fn nonsense_latitude_is_absent() {
        assert_eq!(aircraft_pose(120.0, 0.0, 0.0, 0.0, 0.0, 0.0).valid, 0);
    }
}
