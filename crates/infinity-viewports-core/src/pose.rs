#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Association {
    pub control: u64,
    pub generation: u32,
    pub control_generation: u32,
    pub payload: u64,
}

pub fn slot_is_free(assoc: Association) -> bool {
    if assoc.control == 0 {
        return true;
    }
    if assoc.control_generation != assoc.generation {
        return true;
    }
    assoc.payload == 0
}

#[derive(Clone, Copy, Debug)]
pub struct ViewCand {
    pub w: i32,
    pub h: i32,
    pub ours: bool,
    pub occupied: bool,
}

pub fn is_pilot_window(w: i32, h: i32) -> bool {
    if w < 640 || h < 360 {
        return false;
    }
    let aspect = w as f32 / h as f32;
    (1.2..2.4).contains(&aspect)
}

pub fn pick_pilot<'a>(views: impl IntoIterator<Item = &'a ViewCand>) -> Option<usize> {
    views
        .into_iter()
        .enumerate()
        .filter(|(_, v)| v.occupied && !v.ours && is_pilot_window(v.w, v.h))
        .max_by_key(|(_, v)| v.w as i64 * v.h as i64)
        .map(|(i, _)| i)
}

#[cfg(test)]
pub fn nose_pose(
    position: [f64; 3],
    target: [f64; 3],
    up: [f64; 3],
    forward_m: f64,
) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    if !forward_m.is_finite() || forward_m <= 0.0 {
        return None;
    }
    let d = [
        target[0] - position[0],
        target[1] - position[1],
        target[2] - position[2],
    ];
    if d.iter().any(|c| !c.is_finite()) || up.iter().any(|c| !c.is_finite()) {
        return None;
    }
    let len_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    if len_sq < 1e-6 {
        return None;
    }
    let len = len_sq.sqrt();
    let n = [d[0] / len, d[1] / len, d[2] / len];
    let p = [
        position[0] + n[0] * forward_m,
        position[1] + n[1] * forward_m,
        position[2] + n[2] * forward_m,
    ];
    let t = [p[0] + d[0], p[1] + d[1], p[2] + d[2]];
    Some((p, t, up))
}

pub fn fov_radians(deg: f32) -> f32 {
    deg.to_radians()
}

#[derive(Clone, Copy, Debug)]
pub struct Basis {
    pub origin: [f64; 3],
    pub right: [f64; 3],
    pub up: [f64; 3],
    pub forward: [f64; 3],
}

#[derive(Clone, Copy, Debug)]
pub struct Station {
    pub right_m: f64,
    pub up_m: f64,
    pub forward_m: f64,
    pub yaw_deg: f64,
    pub pitch_deg: f64,
    pub roll_deg: f64,
    pub aim_m: f64,
}

pub fn basis_from_look(position: [f64; 3], target: [f64; 3], up: [f64; 3]) -> Option<Basis> {
    let forward = normalize([
        target[0] - position[0],
        target[1] - position[1],
        target[2] - position[2],
    ])?;
    let right = normalize(cross(forward, up))?;
    let up = normalize(cross(right, forward))?;
    Some(Basis {
        origin: position,
        right,
        up,
        forward,
    })
}

pub fn place(basis: Basis, station: Station) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    if station.aim_m <= 0.0 || !station.aim_m.is_finite() {
        return None;
    }
    let position = add(
        basis.origin,
        add(
            scale(basis.right, station.right_m),
            add(
                scale(basis.up, station.up_m),
                scale(basis.forward, station.forward_m),
            ),
        ),
    );
    if position.iter().any(|c| !c.is_finite()) {
        return None;
    }
    let (forward, _right, up) = aim(basis.forward, basis.right, basis.up, station)?;
    let target = add(position, scale(forward, station.aim_m));
    Some((position, target, up))
}

pub fn basis_from_rows(rows: [[f64; 3]; 4]) -> Option<Basis> {
    let right = normalize(scale(rows[0], -1.0))?;
    let up = normalize(rows[1])?;
    let forward = normalize(rows[2])?;
    if rows[3].iter().any(|c| !c.is_finite()) {
        return None;
    }
    if dot(right, up).abs() > 0.2 || dot(up, forward).abs() > 0.2 || dot(forward, right).abs() > 0.2
    {
        return None;
    }
    Some(Basis {
        origin: rows[3],
        right,
        up,
        forward,
    })
}

pub fn basis_usable(b: &Basis) -> bool {
    if b.origin.iter().any(|c| !c.is_finite()) {
        return false;
    }
    for a in [b.right, b.up, b.forward] {
        if a.iter().any(|c| !c.is_finite()) {
            return false;
        }
        let n = dot(a, a);
        if (n - 1.0).abs() > 1e-3 {
            return false;
        }
    }
    if dot(b.right, b.up).abs() > 0.05
        || dot(b.up, b.forward).abs() > 0.05
        || dot(b.forward, b.right).abs() > 0.05
    {
        return false;
    }
    let swung = cross(b.forward, b.up);
    let d = [
        swung[0] - b.right[0],
        swung[1] - b.right[1],
        swung[2] - b.right[2],
    ];
    dot(d, d) < 1e-3
}

fn aim(
    forward0: [f64; 3],
    right0: [f64; 3],
    up0: [f64; 3],
    station: Station,
) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    let yaw = station.yaw_deg.to_radians();
    let (sy, cy) = (yaw.sin(), yaw.cos());
    let forward1 = normalize(add(scale(forward0, cy), scale(right0, sy)))?;
    let right1 = normalize(add(scale(right0, cy), scale(forward0, -sy)))?;

    let pitch = station.pitch_deg.to_radians();
    let (sp, cp) = (pitch.sin(), pitch.cos());
    let forward2 = normalize(add(scale(forward1, cp), scale(up0, sp)))?;
    let up2 = normalize(add(scale(up0, cp), scale(forward1, -sp)))?;

    let roll = station.roll_deg.to_radians();
    let (sr, cr) = (roll.sin(), roll.cos());
    let right3 = normalize(add(scale(right1, cr), scale(up2, -sr)))?;
    let up3 = normalize(add(scale(up2, cr), scale(right1, sr)))?;
    Some((forward2, right3, up3))
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f64; 3]) -> Option<[f64; 3]> {
    if v.iter().any(|c| !c.is_finite()) {
        return None;
    }
    let len_sq = dot(v, v);
    if len_sq < 1e-12 {
        return None;
    }
    let len = len_sq.sqrt();
    Some(scale(v, 1.0 / len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ(w: i32, h: i32) -> ViewCand {
        ViewCand {
            w,
            h,
            ours: false,
            occupied: true,
        }
    }

    #[test]
    fn free_slot_predicate() {
        assert!(slot_is_free(Association {
            control: 0,
            generation: 1,
            control_generation: 1,
            payload: 1
        }));
        assert!(slot_is_free(Association {
            control: 8,
            generation: 1,
            control_generation: 2,
            payload: 8
        }));
        assert!(slot_is_free(Association {
            control: 8,
            generation: 1,
            control_generation: 1,
            payload: 0
        }));
        assert!(!slot_is_free(Association {
            control: 8,
            generation: 3,
            control_generation: 3,
            payload: 16
        }));
    }

    #[test]
    fn pilot_window_skips_shadow_and_our_view() {
        let views = [
            occ(2048, 2048),
            occ(1024, 576),
            ViewCand {
                w: 2290,
                h: 1262,
                ours: false,
                occupied: true,
            },
            ViewCand {
                w: 1920,
                h: 1080,
                ours: true,
                occupied: true,
            },
            ViewCand {
                w: 2560,
                h: 1440,
                ours: false,
                occupied: false,
            },
        ];
        assert_eq!(pick_pilot(&views), Some(2));
        assert!(is_pilot_window(2290, 1262));
        assert!(!is_pilot_window(2048, 2048));
    }

    #[test]
    fn nose_steps_forward_along_look() {
        let (p, t, up) =
            nose_pose([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 1.0, 0.0], 6.0).unwrap();
        assert!((p[0] - 6.0).abs() < 1e-9 && p[1].abs() < 1e-9 && p[2].abs() < 1e-9);
        assert!((t[0] - 16.0).abs() < 1e-9);
        assert_eq!(up, [0.0, 1.0, 0.0]);
        assert!(nose_pose([1.0, 2.0, 3.0], [1.0, 2.0, 3.0], [0.0, 1.0, 0.0], 6.0).is_none());
    }

    #[test]
    fn station_matches_the_old_forward_step() {
        let basis = basis_from_look([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 1.0, 0.0]).unwrap();
        let (p, t, up) = place(
            basis,
            Station {
                right_m: 0.0,
                up_m: 0.0,
                forward_m: 6.0,
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
                aim_m: 10.0,
            },
        )
        .unwrap();
        assert!((p[0] - 6.0).abs() < 1e-9);
        assert!((t[0] - 16.0).abs() < 1e-9);
        assert!((up[1] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn look_basis_is_usable_and_a_zero_frame_is_not() {
        let basis = basis_from_look([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 1.0, 0.0]).unwrap();
        assert!(basis_usable(&basis));
        assert!(!basis_usable(&Basis {
            origin: [0.0, 0.0, 0.0],
            right: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            forward: [1.0, 0.0, 0.0],
        }));
    }

    #[test]
    fn yaw_turns_toward_mount_right() {
        let basis = Basis {
            origin: [0.0, 0.0, 0.0],
            right: [0.0, 0.0, 1.0],
            up: [0.0, 1.0, 0.0],
            forward: [1.0, 0.0, 0.0],
        };
        let (p, t, _) = place(
            basis,
            Station {
                right_m: 0.0,
                up_m: 0.0,
                forward_m: 0.0,
                yaw_deg: 90.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
                aim_m: 10.0,
            },
        )
        .unwrap();
        assert!(p.iter().all(|c| c.abs() < 1e-9));
        assert!(t[0].abs() < 1e-6 && t[1].abs() < 1e-6 && (t[2] - 10.0).abs() < 1e-6);
    }
}
