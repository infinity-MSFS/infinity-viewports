use crate::camera::AircraftPose;

/// Accept a datum that sits this close to the pilot eye. The eye is not the
/// datum: the gap is the eyepoint, plus geoid and altitude-datum error
pub const ECEF_MATCH_M: f64 = 500.0;

/// A second permutation must miss by at least this much more than the best,
/// or the sample is too close to a coordinate plane to trust
const SEPARATION_M: f64 = 1_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcefMap {
    pub axis: [u8; 3],
    pub sign: [i8; 3],
}

impl EcefMap {
    pub const fn identity() -> Self {
        Self {
            axis: [0, 1, 2],
            sign: [1, 1, 1],
        }
    }

    pub fn apply(self, v: [f64; 3]) -> [f64; 3] {
        [
            self.sign[0] as f64 * v[self.axis[0] as usize],
            self.sign[1] as f64 * v[self.axis[1] as usize],
            self.sign[2] as f64 * v[self.axis[2] as usize],
        ]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EcefFit {
    pub map: EcefMap,
    pub residual_m: f64,
    pub separated: bool,
}

pub fn fit_ecef(standard: [f64; 3], engine: [f64; 3]) -> Option<EcefFit> {
    if standard.iter().any(|c| !c.is_finite()) || engine.iter().any(|c| !c.is_finite()) {
        return None;
    }
    let mut best_d = f64::MAX;
    let mut second = f64::MAX;
    let mut best = EcefMap::identity();
    for axis in PERMS {
        for sign in SIGNS {
            if !proper(axis, sign) {
                continue;
            }
            let d = dist(EcefMap { axis, sign }.apply(standard), engine);
            if d < best_d {
                second = best_d;
                best_d = d;
                best = EcefMap { axis, sign };
            } else if d < second {
                second = d;
            }
        }
    }
    if !best_d.is_finite() {
        return None;
    }
    Some(EcefFit {
        map: best,
        residual_m: best_d,
        separated: second - best_d >= SEPARATION_M,
    })
}

pub fn pose_populated(pose: &AircraftPose) -> bool {
    pose.valid == 1
        && pose.origin.iter().all(|c| c.is_finite())
        && pose.right.iter().all(|c| c.is_finite())
        && pose.up.iter().all(|c| c.is_finite())
        && pose.forward.iter().all(|c| c.is_finite())
}

const PERMS: [[u8; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

const SIGNS: [[i8; 3]; 8] = [
    [1, 1, 1],
    [1, 1, -1],
    [1, -1, 1],
    [1, -1, -1],
    [-1, 1, 1],
    [-1, 1, -1],
    [-1, -1, 1],
    [-1, -1, -1],
];

fn proper(axis: [u8; 3], sign: [i8; 3]) -> bool {
    let mut p = axis;
    let mut swaps = 0u32;
    if p[0] > p[1] {
        p.swap(0, 1);
        swaps += 1;
    }
    if p[1] > p[2] {
        p.swap(1, 2);
        swaps += 1;
    }
    if p[0] > p[1] {
        p.swap(0, 1);
        swaps += 1;
    }
    if p != [0, 1, 2] {
        return false;
    }
    let perm = if swaps % 2 == 0 { 1i32 } else { -1 };
    perm * (sign[0] as i32) * (sign[1] as i32) * (sign[2] as i32) == 1
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    sqrt(d0 * d0 + d1 * d1 + d2 * d2)
}

fn sqrt(x: f64) -> f64 {
    if !(x > 0.0) {
        return 0.0;
    }
    let mut g = 1.0f64;
    while g * g > x && g > 1e-150 {
        g *= 0.5;
    }
    while g * g < x && g < 1e150 {
        g *= 2.0;
    }
    let mut i = 0;
    while i < 8 {
        g = 0.5 * (g + x / g);
        i += 1;
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIENNA: [f64; 3] = [4_092_523.6549, 1_195_484.3519, 4_728_180.7406];

    #[test]
    fn identity_locks_and_a_few_metres_of_eyepoint_still_match() {
        let eye = [VIENNA[0] + 4.0, VIENNA[1] - 2.0, VIENNA[2] + 1.0];
        let fit = fit_ecef(VIENNA, eye).unwrap();
        assert_eq!(fit.map, EcefMap::identity());
        assert!(fit.residual_m < 10.0);
        assert!(fit.separated);
        assert!(fit.residual_m <= ECEF_MATCH_M);
    }

    #[test]
    fn y_up_permutation_is_recovered() {
        let map = EcefMap {
            axis: [0, 2, 1],
            sign: [1, 1, -1],
        };
        let eye = map.apply(VIENNA);
        let fit = fit_ecef(VIENNA, eye).unwrap();
        assert_eq!(fit.map, map);
        assert!(fit.residual_m < 1.0);
        assert!(fit.separated);
        let north = [0.2, -0.4, 0.9];
        let got = fit.map.apply(north);
        assert!((got[0] - north[0]).abs() < 1e-12);
        assert!((got[1] - north[2]).abs() < 1e-12);
        assert!((got[2] + north[1]).abs() < 1e-12);
    }

    #[test]
    fn a_reflection_is_not_a_small_residual() {
        let eye = [-VIENNA[0], VIENNA[1], VIENNA[2]];
        let fit = fit_ecef(VIENNA, eye).unwrap();
        assert!(fit.residual_m > 100_000.0);
        assert!(fit.residual_m > ECEF_MATCH_M);
    }

    #[test]
    fn sqrt_matches_known_values() {
        assert!((sqrt(0.0) - 0.0).abs() < 1e-15);
        assert!((sqrt(4.0) - 2.0).abs() < 1e-12);
        assert!((sqrt(2.0) - 1.41421356237).abs() < 1e-9);
        let earth = 6_371_000.0_f64;
        assert!((sqrt(earth * earth) - earth).abs() < 1e-6);
    }

    #[test]
    fn equator_prime_meridian_is_ambiguous() {
        let std = [6_378_137.0, 0.0, 0.0];
        let fit = fit_ecef(std, std).unwrap();
        assert!(fit.residual_m < 1.0);
        assert!(!fit.separated);
    }
}
