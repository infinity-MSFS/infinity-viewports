use std::ffi::c_void;
use std::time::{Duration, Instant};

use infinity_viewports_protocol::{
    APPLIED_FOLLOW, APPLIED_MISSING, APPLIED_PLACED, CameraStatus, ECEF_MATCH_M, EcefMap, MOUNT_AIRCRAFT,
    NOSE_CREATING, NOSE_LIVE, NOSE_REACQUIRING, NOSE_STOPPED, fit_ecef, pose_populated,
};
use infinity_viewports_shim::{
    Faulted, guarded_activate, guarded_call1, guarded_call1_ret, guarded_call2, guarded_call2_ret,
    guarded_read, guarded_set_fov, guarded_write,
};

use crate::contract::*;
use crate::control::Share;
use crate::mount::{Mount, Mounted};
use crate::pose::{self, Association, Basis, Station, ViewCand};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoseLock {
    Unknown,
    Locked,
    Unmatched,
}

type Call1 = extern "C" fn(*mut c_void);
type Call1Ret = extern "C" fn(*mut c_void) -> *mut c_void;
type Call2 = extern "C" fn(*mut c_void, *mut c_void);
type Call2Ret = extern "C" fn(*mut c_void, *mut c_void) -> u64;
type Activate = extern "C" fn(*mut c_void, u64, i32);
type SetFov = extern "C" fn(*mut c_void, f32);

struct Fns {
    init: Call1,
    activate: Activate,
    create: Call2Ret,
    erase: Call2,
    set_position: Call2,
    set_up: Call2,
    set_target: Call2,
    set_fov: SetFov,
    update_view: Call1,
    refresh: Call1Ret,
}

/// The nose view's life. Nothing but [`Driver::release`] ends it: a view the
/// sim takes away (a flight reload, a renderer reset, a slot reshuffle) or a
/// step that fails goes to `Lost` and is recreated after a backoff, for as
/// long as the core is loaded.
#[derive(Clone, Copy)]
enum Phase {
    Create,
    Ready {
        id: u64,
        waits: u32,
    },
    Live {
        id: u64,
        view: u64,
        node: u64,
        camera: u64,
        renderer: u64,
        since: u64,
        /// Consecutive frames the view failed its health check.
        suspect: u32,
    },
    /// Recreate at frame `at`.
    Lost {
        at: u64,
    },
    Stopped,
}

/// Frames a live view may fail its health check before it counts as lost:
/// rides out a frame where the sim is part-way through changing its tables.
const SUSPECT_FRAMES: u32 = 3;
/// Backoff before recreating, doubling per consecutive failure (about half a
/// second up to ten at 60 fps).
const RETRY_FIRST: u64 = 30;
const RETRY_MAX: u64 = 600;
/// A view live this long clears the failure count.
const STABLE_FRAMES: u64 = 600;
/// Frames to wait for a created entry to come up before erasing it.
const READY_WAITS: u32 = 180;
/// A manager whose entry we hold, not seen updating for this long while
/// another one is: it was destroyed (a flight reload), our entry with it.
const OWNER_GONE: Duration = Duration::from_secs(1);

pub struct Driver {
    base: usize,
    fns: Fns,
    phase: Phase,
    /// Updates of the owning manager (or of any manager while none owns).
    frame: u64,
    /// The view manager our entry lives in; 0 while there is none.
    owner: u64,
    owner_seen: Instant,
    /// Consecutive attempts that ended lost; drives the backoff.
    failures: u32,
    /// Views lost since injection, for the host.
    losses: u16,
    /// What the host is told while the view is not live.
    status_note: String,
    slot_noted: bool,
    waits_for_slot: u32,
    zero_ids: u32,
    sized: bool,
    refresh_noted: bool,
    proj_dumped: bool,
    ladder: [f32; 4],
    clip_logged: Option<(u32, u32, u32)>,
    /// The view's own near plane, from before any near was written, so a
    /// cleared setting gives it back.
    stock_near: Option<f32>,
    mount_note: Option<&'static str>,
    mount_diag_done: bool,
    pose_lock: PoseLock,
    ecef_map: Option<EcefMap>,
    published: Option<CameraStatus>,
    share: Share,
    air: Mount,
    pending_note: Option<String>,
}

impl Driver {
    pub fn installable(base: usize) -> Result<(), String> {
        if image_timestamp(base)? != BUILD_TIMESTAMP {
            return Err(format!(
                "image timestamp is not {BUILD_TIMESTAMP:#x}; refusing this contract"
            ));
        }
        if !prologue_is(base, MANAGER_UPDATE, MANAGER_UPDATE_PROLOGUE)? {
            return Err("manager_update prologue does not match this contract".into());
        }
        if !prologue_is(base, CREATE_ENTRY, CREATE_PROLOGUE)? {
            return Err("create_entry prologue does not match this contract".into());
        }
        let slot = vtable_slot(base)?;
        let expect = base + MANAGER_UPDATE as usize;
        if slot != expect {
            return Err(format!(
                "manager vtable slot is {slot:#x}, contract expects {expect:#x}"
            ));
        }
        Ok(())
    }

    pub fn new(base: usize) -> Driver {
        Driver {
            base,
            fns: Fns {
                init: fn_at(base, INIT_DESCRIPTOR),
                activate: fn_at(base, ACTIVATE_ENTRY),
                create: fn_at(base, CREATE_ENTRY),
                erase: fn_at(base, ERASE_ENTRY),
                set_position: fn_at(base, SET_POSITION),
                set_up: fn_at(base, SET_UP),
                set_target: fn_at(base, SET_TARGET),
                set_fov: fn_at(base, SET_FOV),
                update_view: fn_at(base, UPDATE_VIEW),
                refresh: fn_at(base, REFRESH_OUTPUT),
            },
            phase: Phase::Create,
            frame: 0,
            owner: 0,
            owner_seen: Instant::now(),
            failures: 0,
            losses: 0,
            status_note: "creating the nose view".into(),
            slot_noted: false,
            waits_for_slot: 0,
            zero_ids: 0,
            sized: false,
            refresh_noted: false,
            proj_dumped: false,
            ladder: [f32::NAN; 4],
            clip_logged: None,
            stock_near: None,
            mount_note: None,
            mount_diag_done: false,
            pose_lock: PoseLock::Unknown,
            ecef_map: None,
            published: None,
            share: Share::create(),
            air: Mount::new(base),
            pending_note: None,
        }
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Create => "create",
            Phase::Ready { .. } => "ready",
            Phase::Live { .. } => "live",
            Phase::Lost { .. } => "lost",
            Phase::Stopped => "stopped",
        }
    }

    fn nose_state(&self) -> u16 {
        match self.phase {
            Phase::Live { .. } => NOSE_LIVE,
            Phase::Stopped => NOSE_STOPPED,
            Phase::Lost { .. } => NOSE_REACQUIRING,
            Phase::Create | Phase::Ready { .. } if self.losses > 0 => NOSE_REACQUIRING,
            Phase::Create | Phase::Ready { .. } => NOSE_CREATING,
        }
    }

    pub fn take_note(&mut self) -> Option<String> {
        self.pending_note.take()
    }

    /// Gives the nose view back to the sim: closes and erases the entry this
    /// driver created, so an unload leaves no extra view rendering (and no
    /// slot taken) behind it. Must run where `step` runs, on the sim's
    /// view-manager update. `Ok(false)` when this update is another
    /// manager's and ours is still alive: call again on the next one.
    /// Idempotent.
    pub fn release(&mut self, manager: *mut c_void) -> Result<bool, String> {
        let id = match self.phase {
            Phase::Ready { id, .. } | Phase::Live { id, .. } => id,
            Phase::Create | Phase::Lost { .. } | Phase::Stopped => {
                self.stop("given back for an unload");
                return Ok(true);
            }
        };
        if manager.is_null() {
            return Ok(false);
        }
        if manager as u64 != self.owner {
            if self.owner_seen.elapsed() < OWNER_GONE {
                return Ok(false);
            }
            // Our manager is gone, and the entry with it.
            self.stop("given back for an unload");
            return Ok(true);
        }
        let erased = self.erase_entry(manager, id);
        self.stop("given back for an unload");
        erased?;
        self.note(format!("nose view {id:#x} erased"));
        Ok(true)
    }

    fn stop(&mut self, why: &str) {
        self.phase = Phase::Stopped;
        self.owner = 0;
        self.status_note = why.into();
        self.publish_nose();
    }

    /// Closes and erases entry `id` if it is still in `manager`'s table.
    fn erase_entry(&mut self, manager: *mut c_void, id: u64) -> Result<(), String> {
        if !matches!(find_entry(manager as u64, id), Ok(Some(_))) {
            return Ok(());
        }
        if let Phase::Live { view, node, .. } = self.phase {
            // Undo a parenting before the node goes.
            let _ = self.air.resolve(false, 0, view, node);
        }
        unsafe {
            guarded_activate(self.fns.activate, manager, id, false)
                .map_err(fault("activate(close) on erase"))?;
            guarded_call2(self.fns.erase, manager, id as usize as *mut c_void)
                .map_err(fault("erase_entry"))?;
        }
        Ok(())
    }

    pub fn step(&mut self, manager: *mut c_void) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopped) || manager.is_null() {
            return Ok(());
        }
        let m = manager as u64;
        if self.owner != 0 {
            if m == self.owner {
                self.owner_seen = Instant::now();
            } else if self.owner_seen.elapsed() < OWNER_GONE {
                // Another manager instance's update; ours runs too.
                return Ok(());
            } else {
                // Ours stopped updating: destroyed, and our entry with it.
                // Nothing is left to erase.
                self.owner = 0;
                self.lost(None, "the view manager holding it went away");
                self.publish_nose();
                return Ok(());
            }
        }
        self.frame += 1;
        let result = match self.phase {
            Phase::Create => self.try_create(manager),
            Phase::Ready { id, waits } => self.try_ready(manager, id, waits),
            Phase::Live {
                id,
                view,
                node,
                camera,
                renderer,
                since,
                suspect,
            } => self.track(manager, id, view, node, camera, renderer, since, suspect),
            Phase::Lost { at } => {
                if self.frame >= at {
                    self.phase = Phase::Create;
                    self.status_note = "recreating the nose view".into();
                }
                Ok(())
            }
            Phase::Stopped => Ok(()),
        };
        if let Err(err) = &result {
            let err = err.clone();
            self.lost(Some(manager), &err);
        }
        self.publish_nose();
        result
    }

    /// The view is gone or failed: erase what is left of it (when `manager`
    /// still holds it) and schedule a fresh one.
    fn lost(&mut self, manager: Option<*mut c_void>, why: &str) {
        if let (Some(manager), Phase::Ready { id, .. } | Phase::Live { id, .. }) =
            (manager, self.phase)
            && manager as u64 == self.owner
            && let Err(err) = self.erase_entry(manager, id)
        {
            self.note(format!("could not erase the lost entry {id:#x}: {err}"));
        }
        self.failures = self.failures.saturating_add(1);
        self.losses = self.losses.saturating_add(1);
        let delay = retry_delay(self.failures);
        self.phase = Phase::Lost {
            at: self.frame + delay,
        };
        self.owner = 0;
        self.reset_view_state();
        self.status_note = format!("nose view lost ({why}); reacquiring");
        self.note(format!(
            "nose view lost: {why}; recreating in {delay} frames (attempt {})",
            self.failures
        ));
    }

    /// Forgets everything that belonged to the old view.
    fn reset_view_state(&mut self) {
        self.waits_for_slot = 0;
        self.zero_ids = 0;
        self.slot_noted = false;
        self.sized = false;
        self.refresh_noted = false;
        self.ladder = [f32::NAN; 4];
        self.clip_logged = None;
        self.stock_near = None;
        self.mount_note = None;
        self.mount_diag_done = false;
        self.air = Mount::new(self.base);
        self.published = None;
    }

    /// Tells the host the view's state when it changes. While live,
    /// [`publish_status`](Self::publish_status) carries it with the pose.
    fn publish_nose(&mut self) {
        let nose = self.nose_state();
        let mut status = self.published.unwrap_or_else(CameraStatus::empty);
        status.nose = nose;
        status.nose_losses = self.losses;
        if nose != NOSE_LIVE {
            status.applied = APPLIED_MISSING;
            status.parented = 0;
            status.set_note(&self.status_note);
        }
        if self
            .published
            .as_ref()
            .is_some_and(|prev| status_same(prev, &status))
        {
            return;
        }
        self.share.publish(status);
        self.published = Some(status);
    }

    fn try_create(&mut self, manager: *mut c_void) -> Result<(), String> {
        let Some(renderer) = read_global(self.base, RENDERER_GLOBAL)? else {
            return Ok(());
        };
        if free_count(renderer)? == 0 {
            // The sim's own views hold every slot: one frees up eventually,
            // so keep waiting rather than give up.
            self.waits_for_slot += 1;
            if self.waits_for_slot > 300 && !self.slot_noted {
                self.slot_noted = true;
                self.status_note = "waiting for the sim to free a scene-view slot".into();
                self.note("every scene-view slot is taken; waiting for one".into());
            }
            return Ok(());
        }
        #[repr(C, align(16))]
        struct Desc([u8; 256]);
        let mut desc = Desc([0u8; 256]);
        unsafe {
            guarded_call1(self.fns.init, desc.0.as_mut_ptr().cast())
                .map_err(fault("initialize_descriptor"))?
        };
        desc.0[40] = MODE_INDEPENDENT as u8;
        desc.0[44] = 1;
        let id = unsafe {
            guarded_call2_ret(self.fns.create, manager, desc.0.as_mut_ptr().cast())
                .map_err(fault("create_entry"))?
        };
        if id == 0 {
            self.zero_ids += 1;
            if self.zero_ids > 30 {
                return Err("create_entry returned no id".into());
            }
            return Ok(());
        }
        self.note(format!("created entry {id:#x}"));
        self.owner = manager as u64;
        self.owner_seen = Instant::now();
        self.phase = Phase::Ready { id, waits: 0 };
        Ok(())
    }

    fn try_ready(&mut self, manager: *mut c_void, id: u64, waits: u32) -> Result<(), String> {
        if waits > READY_WAITS {
            // `lost` erases it.
            return Err(format!("entry {id:#x} never became ready"));
        }
        let Some(entry) = find_entry(manager as u64, id)? else {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        };
        let ready = read_u8(entry + ENTRY_READY)?;
        if ready == 0 {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        }
        if ready != 1 {
            return Err(format!("entry {id:#x} ready byte is {ready}"));
        }
        let mode = read_u32(entry + ENTRY_MODE)?;
        if mode != MODE_INDEPENDENT {
            return Err(format!(
                "entry {id:#x} mode is {mode}, want {MODE_INDEPENDENT}"
            ));
        }
        let index = read_i32(entry + ENTRY_VIEW_INDEX)?;
        if !(0..VIEW_SLOTS as i32).contains(&index) {
            return Err(format!(
                "entry {id:#x} view index {index} is outside the pool"
            ));
        }
        let Some(renderer) = read_global(self.base, RENDERER_GLOBAL)? else {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        };
        let array = read_u64(renderer + RENDERER_VIEW_ARRAY)?;
        let view = read_u64(array + index as u64 * 8)?;
        let node = resolve_handle(entry + ENTRY_NODE_HANDLE)?;
        let view_node = resolve_handle(view + VIEW_NODE_HANDLE)?;
        if node == 0 || node != view_node {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        }
        let camera = read_u64(node + NODE_CAMERA)?;
        let kind = read_u16(camera + CAMERA_TYPE)?;
        if kind != CAMERA_TYPE_VALUE {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        }

        unsafe {
            guarded_activate(self.fns.activate, manager, id, false)
                .map_err(fault("activate(close)"))?
        };
        let flags = read_u64(view + VIEW_FLAGS)?;
        if flags & 1 == 0 {
            self.phase = Phase::Ready {
                id,
                waits: waits + 1,
            };
            return Ok(());
        }
        if !self.sized {
            write_nose_size(view)?;
            let control = self.share.read();
            self.sync_camera_clip(renderer, view, camera, control.near_m, control.far_m)?;
            unsafe {
                guarded_call1(self.fns.update_view, view as *mut c_void)
                    .map_err(fault("update_view"))?
            };
            let produced = unsafe {
                guarded_call1_ret(self.fns.refresh, view as *mut c_void)
                    .map_err(fault("refresh_output"))?
            };
            if produced as u64 != view + VIEW_OUTPUT {
                self.phase = Phase::Ready {
                    id,
                    waits: waits + 1,
                };
                if !self.refresh_noted {
                    self.refresh_noted = true;
                    self.note(format!(
                        "refresh_output returned {produced:?}, want view+{VIEW_OUTPUT}; retrying"
                    ));
                }
                return Ok(());
            }
            self.sized = true;
        }
        self.apply_pose(view, node, camera)?;
        unsafe {
            guarded_activate(self.fns.activate, manager, id, true)
                .map_err(fault("activate(open)"))?
        };
        self.note(format!("nose view live id={id:#x} view={view:#x} camera={camera:#x} size {NOSE_WIDTH}x{NOSE_HEIGHT}"));
        if self.failures > 0 {
            self.note(format!(
                "nose view reacquired ({} lost since injection)",
                self.losses
            ));
        }
        self.phase = Phase::Live {
            id,
            view,
            node,
            camera,
            renderer,
            since: self.frame,
            suspect: 0,
        };
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn track(
        &mut self,
        manager: *mut c_void,
        id: u64,
        view: u64,
        node: u64,
        camera: u64,
        renderer: u64,
        since: u64,
        suspect: u32,
    ) -> Result<(), String> {
        let health = still_ours(self.base, manager as u64, id, view, node, camera, renderer);
        let suspect = match health {
            Ok(()) => 0,
            Err(why) if suspect + 1 >= SUSPECT_FRAMES => return Err(why),
            Err(_) => suspect + 1,
        };
        self.phase = Phase::Live {
            id,
            view,
            node,
            camera,
            renderer,
            since,
            suspect,
        };
        if suspect != 0 {
            // Not posed while in doubt: the node may already be freed.
            return Ok(());
        }
        if self.failures > 0 && self.frame.saturating_sub(since) > STABLE_FRAMES {
            self.failures = 0;
        }
        if !self.proj_dumped {
            self.proj_dumped = true;
            self.pending_note = Some(self.projection_dump(view, camera));
        }
        if let Ok(cur) = read_ladder(view) {
            if cur
                .iter()
                .zip(self.ladder)
                .any(|(a, b)| a.to_bits() != b.to_bits())
            {
                self.ladder = cur;
                if self.pending_note.is_none() {
                    self.pending_note = Some(format!(
                        "view+0x460 ladder: {:.2} {:.2} {:.2} {:.2}",
                        cur[0], cur[1], cur[2], cur[3]
                    ));
                }
            }
        }
        self.apply_pose(view, node, camera)
    }

    fn projection_dump(&self, our_view: u64, nose_camera: u64) -> String {
        let Some(renderer) = read_global(self.base, RENDERER_GLOBAL).ok().flatten() else {
            return "view dump: no renderer".into();
        };
        let Some((pilot_view, pilot_camera)) = pilot_pick(renderer, our_view).ok().flatten() else {
            return format!("view dump: no pilot view resolved (nose view {our_view:#x})");
        };
        let nfov = read_f32(nose_camera + CAMERA_FOV).unwrap_or(f32::NAN);
        let pfov = read_f32(pilot_camera + CAMERA_FOV).unwrap_or(f32::NAN);
        let nclip = read_clip(nose_camera).unwrap_or([f32::NAN; 3]);
        let pclip = read_clip(pilot_camera).unwrap_or([f32::NAN; 3]);
        let mut lines = vec![
            format!("view dump  nose view {our_view:#x}  vs  pilot view {pilot_view:#x}"),
            format!("  cam FOV  nose {nfov:.5} | pilot {pfov:.5}  (anchor)"),
            format!(
                "  cam clip near/far/default  nose {:.3}/{:.3}/{:.3} | pilot {:.3}/{:.3}/{:.3}",
                nclip[0], nclip[1], nclip[2], pclip[0], pclip[1], pclip[2]
            ),
            "  differing plausible f32 (off: nose | pilot ; also as f64):".into(),
        ];
        lines.extend(diff_plausible_f32(our_view, pilot_view, 0x00, 0x600));
        lines.join("\n")
    }

    fn apply_pose(&mut self, our_view: u64, node: u64, camera: u64) -> Result<(), String> {
        let Some(renderer) = read_global(self.base, RENDERER_GLOBAL)? else {
            return Ok(());
        };
        let Some((pilot_node, eye, look_at, look_up)) = pilot_pose(renderer, our_view)? else {
            return Ok(());
        };
        let node = if node != 0 {
            node
        } else {
            resolve_handle(our_view + VIEW_NODE_HANDLE)?
        };
        if node == 0 {
            return Ok(());
        }
        let control = self.share.read();
        let on_aircraft = control.mount == MOUNT_AIRCRAFT;
        let sim = if on_aircraft {
            self.sim_basis(eye)
        } else {
            None
        };
        // The aircraft model's transform, read in this update: the camera
        // moves with the drawn aircraft. The SimConnect pose arrives on the
        // manager's clock and lags the frame by a varying amount, which
        // shows as the nose jittering in the picture; it only checks the
        // model transform, and stands in when there is none.
        let frame = if on_aircraft {
            self.air.frame_basis(pilot_node)
        } else {
            None
        };
        if on_aircraft && frame.is_none() {
            if !self.mount_diag_done {
                self.mount_diag_done = true;
                let diag = self.air.diagnose(pilot_node);
                self.note(diag);
            }
        } else {
            self.mount_diag_done = false;
        }
        let agreed = frame.filter(|f| sim.is_none_or(|s| pose::bases_agree(f, &s)));
        let (basis, mounted) = if !on_aircraft {
            (
                pose::basis_from_look(eye, look_at, look_up),
                Mounted {
                    applied: APPLIED_FOLLOW,
                    parented: 0,
                    note: "following the pilot view",
                },
            )
        } else if let Some(f) = agreed {
            (
                Some(f),
                Mounted {
                    applied: APPLIED_PLACED,
                    parented: 0,
                    note: "on the aircraft model, same frame",
                },
            )
        } else if let Some(s) = sim {
            (
                Some(s),
                Mounted {
                    applied: APPLIED_PLACED,
                    parented: 0,
                    note: if frame.is_some() {
                        "on the sim pose; the model transform disagreed with it"
                    } else {
                        "on the sim pose (no model transform; lags the frame)"
                    },
                },
            )
        } else {
            (
                None,
                Mounted {
                    applied: APPLIED_MISSING,
                    parented: 0,
                    note: "no aircraft pose; camera held",
                },
            )
        };
        let on_body = on_aircraft && basis.is_some();
        let mut origin = eye;
        if let Some(basis) = basis {
            let aim = if control.aim_m > 0.0 {
                control.aim_m as f64
            } else if on_body {
                10_000.0
            } else {
                look_length(eye, look_at)
            };
            if let Some((position, target, up)) = pose::place(
                basis,
                Station {
                    right_m: control.right_m as f64,
                    up_m: control.up_m as f64,
                    forward_m: control.forward_m as f64,
                    yaw_deg: control.yaw_deg as f64,
                    pitch_deg: control.pitch_deg as f64,
                    roll_deg: control.roll_deg as f64,
                    aim_m: aim,
                },
            ) {
                origin = position;
                unsafe {
                    guarded_call2(
                        self.fns.set_position,
                        node as *mut c_void,
                        position.as_ptr() as *mut c_void,
                    )
                    .map_err(fault("set_position"))?;
                    guarded_call2(
                        self.fns.set_up,
                        camera as *mut c_void,
                        up.as_ptr() as *mut c_void,
                    )
                    .map_err(fault("set_up"))?;
                    guarded_call2(
                        self.fns.set_target,
                        camera as *mut c_void,
                        target.as_ptr() as *mut c_void,
                    )
                    .map_err(fault("set_target"))?;
                }
            }
        }
        unsafe {
            guarded_set_fov(
                self.fns.set_fov,
                camera as *mut c_void,
                pose::fov_radians(control.fov_deg),
            )
            .map_err(fault("set_fov"))?;
        }
        let (near, far) =
            self.sync_camera_clip(renderer, our_view, camera, control.near_m, control.far_m)?;
        unsafe {
            guarded_call1(self.fns.update_view, our_view as *mut c_void)
                .map_err(fault("update_view"))?
        };
        self.publish_status(&mounted, control.fov_deg, near, far, origin);
        Ok(())
    }

    fn sim_basis(&mut self, eye: [f64; 3]) -> Option<Basis> {
        let pose = self.share.read_pose();
        if !pose_populated(&pose) {
            return None;
        }
        let fit = fit_ecef(pose.origin, eye)?;
        let cached_ok = self.ecef_map.is_some_and(|map| {
            let d = dist3(map.apply(pose.origin), eye);
            d <= ECEF_MATCH_M
        });
        let fresh_map = fit.residual_m <= ECEF_MATCH_M && fit.separated;
        let previous = self.ecef_map;
        if fresh_map {
            self.ecef_map = Some(fit.map);
        } else if !cached_ok {
            if self.pose_lock != PoseLock::Unmatched {
                self.pose_lock = PoseLock::Unmatched;
                self.ecef_map = None;
                if fit.residual_m <= ECEF_MATCH_M {
                    self.note(
                        "sim pose ambiguous; the eye is too close to an ECEF axis to pick the frame"
                            .into(),
                    );
                } else {
                    self.note(format!(
                        "sim pose unmatched, best {:.0} m from the eye",
                        fit.residual_m
                    ));
                }
            }
            return None;
        }
        let map = self.ecef_map?;
        let basis = Basis {
            origin: map.apply(pose.origin),
            right: map.apply(pose.right),
            up: map.apply(pose.up),
            forward: map.apply(pose.forward),
        };
        if dist3(basis.origin, eye) > ECEF_MATCH_M || !pose::basis_usable(&basis) {
            if self.pose_lock != PoseLock::Unmatched {
                self.note(format!(
                    "sim pose rejected, {:.0} m from the eye",
                    dist3(basis.origin, eye)
                ));
            }
            self.ecef_map = None;
            self.pose_lock = PoseLock::Unmatched;
            return None;
        }
        if fresh_map && (self.pose_lock != PoseLock::Locked || previous != Some(fit.map)) {
            self.pose_lock = PoseLock::Locked;
            self.note(format!(
                "sim pose locked, {:.1} m from the eye, axes {}",
                fit.residual_m,
                axes_label(fit.map)
            ));
        }
        Some(basis)
    }

    fn publish_status(
        &mut self,
        mounted: &Mounted,
        fov: f32,
        near: f32,
        far: f32,
        origin: [f64; 3],
    ) {
        if self.mount_note != Some(mounted.note) {
            self.mount_note = Some(mounted.note);
            self.note(format!("camera mount: {}", mounted.note));
        }
        let mut status = CameraStatus::empty();
        status.applied = mounted.applied;
        status.parented = mounted.parented;
        status.fov_deg = fov;
        status.near_m = near;
        status.far_m = far;
        status.origin = origin;
        status.set_note(mounted.note);
        status.nose = self.nose_state();
        status.nose_losses = self.losses;
        if self
            .published
            .as_ref()
            .is_some_and(|prev| status_same(prev, &status))
        {
            return;
        }
        self.share.publish(status);
        self.published = Some(status);
    }

    fn sync_camera_clip(
        &mut self,
        renderer: u64,
        our_view: u64,
        camera: u64,
        near_m: f32,
        far_m: f32,
    ) -> Result<(f32, f32), String> {
        let nose = read_clip(camera)?;
        let (want_far, want_default, manual) = if far_m > 0.0 {
            (far_m, far_m, true)
        } else if let Some((_, pilot_camera)) = pilot_pick(renderer, our_view)? {
            let pilot = read_clip(pilot_camera)?;
            if plausible_clip(pilot) {
                (pilot[1], pilot[2], false)
            } else {
                (nose[1], nose[2], false)
            }
        } else {
            (nose[1], nose[2], false)
        };
        let stock_near = *self.stock_near.get_or_insert(nose[0]);
        let want_near = if near_m > 0.0 { near_m } else { stock_near };
        if !plausible_clip([want_near, want_far, want_default]) {
            return Ok((nose[0], nose[1]));
        }
        if want_near.to_bits() != nose[0].to_bits() {
            write_camera_near(camera, want_near)?;
        }
        if want_far.to_bits() != nose[1].to_bits() || want_default.to_bits() != nose[2].to_bits() {
            write_camera_far(camera, want_far, want_default)?;
        }
        let key = (
            want_near.to_bits(),
            want_far.to_bits(),
            want_default.to_bits(),
        );
        if self.clip_logged != Some(key) {
            self.clip_logged = Some(key);
            let far_same = want_far.to_bits() == nose[1].to_bits()
                && want_default.to_bits() == nose[2].to_bits();
            let why = if manual {
                "manual far"
            } else if near_m > 0.0 && want_near.to_bits() != nose[0].to_bits() && far_same {
                "wrote near"
            } else if far_same {
                "already matched"
            } else {
                "wrote pilot far"
            };
            self.note(format!(
                "camera clip near/cull/default {want_near:.3}/{want_far:.3}/{want_default:.3} ({why})"
            ));
        }
        Ok((want_near, want_far))
    }

    fn note(&mut self, line: String) {
        self.pending_note = Some(match self.pending_note.take() {
            Some(prev) => format!("{prev}\n{line}"),
            None => line,
        });
    }
}

fn plausible_clip(c: [f32; 3]) -> bool {
    c[0].is_finite()
        && c[1].is_finite()
        && c[2].is_finite()
        && c[0] > 0.0
        && c[2] > c[0]
        && (c[1] < 0.0 || c[1] > c[0])
}

fn read_clip(camera: u64) -> Result<[f32; 3], String> {
    Ok([
        read_f32(camera + CAMERA_NEAR)?,
        read_f32(camera + CAMERA_FAR)?,
        read_f32(camera + CAMERA_DEFAULT_FAR)?,
    ])
}

fn write_camera_near(camera: u64, near: f32) -> Result<(), String> {
    let bytes = near.to_le_bytes();
    unsafe {
        guarded_write((camera + CAMERA_NEAR) as *mut u8, &bytes)
            .map_err(fault("write camera near"))?
    };
    Ok(())
}

fn write_camera_far(camera: u64, far: f32, default_far: f32) -> Result<(), String> {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&far.to_le_bytes());
    bytes[4..].copy_from_slice(&default_far.to_le_bytes());
    unsafe {
        guarded_write((camera + CAMERA_FAR) as *mut u8, &bytes)
            .map_err(fault("write camera far"))?
    };
    Ok(())
}

fn write_nose_size(view: u64) -> Result<(), String> {
    let mut bytes = [0u8; 24];
    for pair in 0..3 {
        bytes[pair * 8..pair * 8 + 4].copy_from_slice(&NOSE_WIDTH.to_le_bytes());
        bytes[pair * 8 + 4..pair * 8 + 8].copy_from_slice(&NOSE_HEIGHT.to_le_bytes());
    }
    unsafe {
        guarded_write((view + VIEW_DIMENSIONS) as *mut u8, &bytes)
            .map_err(fault("write dimensions"))?
    };
    Ok(())
}

fn axes_label(map: EcefMap) -> String {
    let one = |i: usize| format!("{}{}", map.axis[i], if map.sign[i] < 0 { '-' } else { '+' });
    format!("{} {} {}", one(0), one(1), one(2))
}

fn dist3(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    (d0 * d0 + d1 * d1 + d2 * d2).sqrt()
}

fn look_length(eye: [f64; 3], look_at: [f64; 3]) -> f64 {
    let d = [
        look_at[0] - eye[0],
        look_at[1] - eye[1],
        look_at[2] - eye[2],
    ];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if len.is_finite() && len >= 1.0 {
        len
    } else {
        1.0
    }
}

fn status_same(a: &CameraStatus, b: &CameraStatus) -> bool {
    a.applied == b.applied
        && a.parented == b.parented
        && a.fov_deg.to_bits() == b.fov_deg.to_bits()
        && a.near_m.to_bits() == b.near_m.to_bits()
        && a.far_m.to_bits() == b.far_m.to_bits()
        && a.nose == b.nose
        && a.nose_losses == b.nose_losses
        && a.note == b.note
}

fn pilot_pose(
    renderer: u64,
    our_view: u64,
) -> Result<Option<(u64, [f64; 3], [f64; 3], [f64; 3])>, String> {
    let array = read_u64(renderer + RENDERER_VIEW_ARRAY)?;
    let mut cands = [ViewCand {
        w: 0,
        h: 0,
        ours: false,
        occupied: false,
    }; VIEW_SLOTS];
    let mut nodes = [0u64; VIEW_SLOTS];
    let mut cameras = [0u64; VIEW_SLOTS];
    for i in 0..VIEW_SLOTS {
        let view = read_u64(array + i as u64 * 8)?;
        let (w, h) = read_dimensions(view + VIEW_DIMENSIONS)?;
        let assoc = read_association(view + VIEW_ASSOCIATION)?;
        let occupied = !pose::slot_is_free(assoc);
        let node = if occupied {
            resolve_handle(view + VIEW_NODE_HANDLE).unwrap_or(0)
        } else {
            0
        };
        let camera = if node != 0 {
            read_u64(node + NODE_CAMERA).unwrap_or(0)
        } else {
            0
        };
        nodes[i] = node;
        cameras[i] = camera;
        cands[i] = ViewCand {
            w,
            h,
            ours: view == our_view,
            occupied: occupied && camera != 0,
        };
    }
    let Some(index) = pose::pick_pilot(cands.iter()) else {
        return Ok(None);
    };
    let node = nodes[index];
    let camera = cameras[index];
    if node == 0 || camera == 0 {
        return Ok(None);
    }
    let position = read_f64x3(node + NODE_POSITION)?;
    let target = read_f64x3(camera + CAMERA_TARGET)?;
    let up = read_f64x3(camera + CAMERA_UP)?;
    Ok(Some((node, position, target, up)))
}

fn pilot_pick(renderer: u64, our_view: u64) -> Result<Option<(u64, u64)>, String> {
    let array = read_u64(renderer + RENDERER_VIEW_ARRAY)?;
    let mut cands = [ViewCand {
        w: 0,
        h: 0,
        ours: false,
        occupied: false,
    }; VIEW_SLOTS];
    let mut views = [0u64; VIEW_SLOTS];
    let mut cameras = [0u64; VIEW_SLOTS];
    for i in 0..VIEW_SLOTS {
        let view = read_u64(array + i as u64 * 8)?;
        let (w, h) = read_dimensions(view + VIEW_DIMENSIONS)?;
        let assoc = read_association(view + VIEW_ASSOCIATION)?;
        let occupied = !pose::slot_is_free(assoc);
        let node = if occupied {
            resolve_handle(view + VIEW_NODE_HANDLE).unwrap_or(0)
        } else {
            0
        };
        let camera = if node != 0 {
            read_u64(node + NODE_CAMERA).unwrap_or(0)
        } else {
            0
        };
        views[i] = view;
        cameras[i] = camera;
        cands[i] = ViewCand {
            w,
            h,
            ours: view == our_view,
            occupied: occupied && camera != 0,
        };
    }
    let Some(index) = pose::pick_pilot(cands.iter()) else {
        return Ok(None);
    };
    let (view, camera) = (views[index], cameras[index]);
    Ok((camera != 0).then_some((view, camera)))
}

fn diff_plausible_f32(a: u64, b: u64, start: u64, end: u64) -> Vec<String> {
    let mut out = Vec::new();
    let mut off = start;
    while off < end {
        if let (Ok(na), Ok(pb)) = (read_f32(a + off), read_f32(b + off)) {
            let m = na.abs().max(pb.abs());
            if na.is_finite()
                && pb.is_finite()
                && (na - pb).abs() > 1e-3
                && (1.0..1.0e8).contains(&m)
            {
                let ad = read_f64(a + (off & !7)).unwrap_or(f64::NAN);
                let bd = read_f64(b + (off & !7)).unwrap_or(f64::NAN);
                out.push(format!(
                    "    {off:#05x}: {na:.3} | {pb:.3}   (f64@{:#05x} {ad:.3} | {bd:.3})",
                    off & !7
                ));
            }
        }
        off += 4;
    }
    if out.is_empty() {
        out.push("    (none)".into());
    }
    out
}

/// Frames to wait before the `failures`-th consecutive recreate.
fn retry_delay(failures: u32) -> u64 {
    (RETRY_FIRST << failures.saturating_sub(1).min(8)).min(RETRY_MAX)
}

/// Whether the live view is still the one we made: the entry still in the
/// table and ready, on the same slot, bound to the same node and camera, in
/// the same renderer. Otherwise, why not.
fn still_ours(
    base: usize,
    manager: u64,
    id: u64,
    view: u64,
    node: u64,
    camera: u64,
    renderer: u64,
) -> Result<(), String> {
    if read_global(base, RENDERER_GLOBAL)? != Some(renderer) {
        return Err("the renderer was replaced".into());
    }
    let Some(entry) = find_entry(manager, id)? else {
        return Err(format!("the sim removed entry {id:#x}"));
    };
    if read_u8(entry + ENTRY_READY)? != 1 {
        return Err(format!("entry {id:#x} is no longer ready"));
    }
    let index = read_i32(entry + ENTRY_VIEW_INDEX)?;
    if !(0..VIEW_SLOTS as i32).contains(&index) {
        return Err(format!("entry {id:#x} lost its view slot"));
    }
    let array = read_u64(renderer + RENDERER_VIEW_ARRAY)?;
    if read_u64(array + index as u64 * 8)? != view {
        return Err(format!("entry {id:#x} moved to another view"));
    }
    if resolve_handle(entry + ENTRY_NODE_HANDLE)? != node
        || resolve_handle(view + VIEW_NODE_HANDLE)? != node
    {
        return Err("the view's camera node was released".into());
    }
    if read_u64(node + NODE_CAMERA)? != camera
        || read_u16(camera + CAMERA_TYPE)? != CAMERA_TYPE_VALUE
    {
        return Err("the view's camera was replaced".into());
    }
    Ok(())
}

fn free_count(renderer: u64) -> Result<usize, String> {
    let array = read_u64(renderer + RENDERER_VIEW_ARRAY)?;
    let mut free = 0usize;
    for i in 0..VIEW_SLOTS {
        let view = read_u64(array + i as u64 * 8)?;
        if view == 0 {
            return Err("scene-view pool contains a null view".into());
        }
        let assoc = read_association(view + VIEW_ASSOCIATION)?;
        if pose::slot_is_free(assoc) {
            free += 1;
        }
    }
    Ok(free)
}

fn find_entry(manager: u64, id: u64) -> Result<Option<u64>, String> {
    let count = read_u32(manager + ENTRY_TABLE)? as usize;
    let buckets = read_u32(manager + ENTRY_TABLE + 4)? as usize;
    let heads = read_u64(manager + ENTRY_TABLE + 8)?;
    if buckets == 0 || buckets > 4096 || count > 4096 {
        return Ok(None);
    }
    let mut visited = 0usize;
    for b in 0..buckets {
        let mut link = read_u64(heads + b as u64 * 8)?;
        let mut guard = 0u32;
        while link != 0 && guard < 1024 {
            let addr = link & !1;
            if addr < 0x10000 {
                return Err("entry link is not a pointer".into());
            }
            let key = read_u64(addr)?;
            let payload = read_u64(addr + 16)?;
            if key == id && payload == id {
                return Ok(Some(addr));
            }
            link = read_u64(addr + ENTRY_NEXT)?;
            guard += 1;
            visited += 1;
            if visited > 4096 {
                return Ok(None);
            }
        }
    }
    Ok(None)
}

fn read_association(addr: u64) -> Result<Association, String> {
    let control = read_u64(addr)?;
    let generation = read_u32(addr + 8)?;
    if control == 0 {
        return Ok(Association {
            control: 0,
            generation,
            control_generation: 0,
            payload: 0,
        });
    }
    let control_generation = read_u32(control + 28)?;
    let payload = if control_generation == generation {
        read_u64(control)?
    } else {
        0
    };
    Ok(Association {
        control,
        generation,
        control_generation,
        payload,
    })
}

fn resolve_handle(addr: u64) -> Result<u64, String> {
    let assoc = read_association(addr)?;
    if pose::slot_is_free(assoc) {
        Ok(0)
    } else {
        Ok(assoc.payload)
    }
}

fn read_dimensions(addr: u64) -> Result<(i32, i32), String> {
    Ok((read_i32(addr)?, read_i32(addr + 4)?))
}

fn read_f64x3(addr: u64) -> Result<[f64; 3], String> {
    Ok([read_f64(addr)?, read_f64(addr + 8)?, read_f64(addr + 16)?])
}

fn read_global(base: usize, rva: u32) -> Result<Option<u64>, String> {
    let v = read_u64(base as u64 + rva as u64)?;
    Ok((v != 0).then_some(v))
}

fn image_timestamp(base: usize) -> Result<u32, String> {
    let mut dos = [0u8; 0x40];
    unsafe { guarded_read(&mut dos, base as *const u8).map_err(fault("pe header"))? };
    if &dos[..2] != b"MZ" {
        return Err("main image is not MZ".into());
    }
    let pe = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as usize;
    let mut ts = [0u8; 4];
    unsafe { guarded_read(&mut ts, (base + pe + 8) as *const u8).map_err(fault("pe timestamp"))? };
    Ok(u32::from_le_bytes(ts))
}

fn prologue_is(base: usize, rva: u32, expect: &[u8]) -> Result<bool, String> {
    let mut got = vec![0u8; expect.len()];
    unsafe {
        guarded_read(&mut got, (base + rva as usize) as *const u8).map_err(fault("prologue"))?
    };
    Ok(got == expect)
}

fn vtable_slot(base: usize) -> Result<usize, String> {
    let addr = base as u64 + MANAGER_VTABLE as u64 + (MANAGER_UPDATE_SLOT * 8) as u64;
    Ok(read_u64(addr)? as usize)
}

fn fn_at<T: Copy>(base: usize, rva: u32) -> T {
    let addr = base + rva as usize;
    assert_eq!(size_of::<T>(), size_of::<usize>());
    unsafe { std::mem::transmute_copy(&addr) }
}

fn fault(what: &'static str) -> impl Fn(Faulted) -> String {
    move |_| format!("{what} faulted")
}

fn read_bytes<const N: usize>(addr: u64) -> Result<[u8; N], String> {
    let mut buf = [0u8; N];
    unsafe { guarded_read(&mut buf, addr as *const u8).map_err(fault("read"))? };
    Ok(buf)
}

fn read_u8(addr: u64) -> Result<u8, String> {
    Ok(read_bytes::<1>(addr)?[0])
}
fn read_u16(addr: u64) -> Result<u16, String> {
    Ok(u16::from_le_bytes(read_bytes::<2>(addr)?))
}
fn read_u32(addr: u64) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_bytes::<4>(addr)?))
}
fn read_i32(addr: u64) -> Result<i32, String> {
    Ok(i32::from_le_bytes(read_bytes::<4>(addr)?))
}
fn read_u64(addr: u64) -> Result<u64, String> {
    Ok(u64::from_le_bytes(read_bytes::<8>(addr)?))
}
fn read_f32(addr: u64) -> Result<f32, String> {
    Ok(f32::from_le_bytes(read_bytes::<4>(addr)?))
}
/// Read-only: the four f32 at `view+0x460`. Not the camera far plane.
fn read_ladder(view: u64) -> Result<[f32; 4], String> {
    Ok([
        read_f32(view + 0x460)?,
        read_f32(view + 0x464)?,
        read_f32(view + 0x468)?,
        read_f32(view + 0x46C)?,
    ])
}
fn read_f64(addr: u64) -> Result<f64, String> {
    Ok(f64::from_le_bytes(read_bytes::<8>(addr)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recreating_backs_off_to_a_ceiling_and_never_stops() {
        assert_eq!(retry_delay(1), RETRY_FIRST);
        assert_eq!(retry_delay(2), RETRY_FIRST * 2);
        assert_eq!(retry_delay(3), RETRY_FIRST * 4);
        assert_eq!(retry_delay(10), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
        assert_eq!(retry_delay(0), RETRY_FIRST);
    }
}