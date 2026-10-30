pub const BUILD_TIMESTAMP: u32 = 0x6AD6_56BB;

pub const INIT_DESCRIPTOR: u32 = 0x0121_50E0;
pub const ACTIVATE_ENTRY: u32 = 0x0121_56E0;
pub const CREATE_ENTRY: u32 = 0x0121_6B30;
pub const ERASE_ENTRY: u32 = 0x0121_6760;
pub const SET_POSITION: u32 = 0x0415_32D0;
/// `attach_child(parent, handle, keep_world)` and `detach_node(node, keep_children, keep_world)`.
/// Recorded for this image. The driver calls them only when the bytes at the
/// RVA look like a function; otherwise the camera stays on its original parent.
pub const ATTACH_CHILD: u32 = 0x0414_DCD0;
pub const DETACH_NODE: u32 = 0x0415_3010;
pub const SET_UP: u32 = 0x041C_2DA0;
pub const SET_TARGET: u32 = 0x041C_2D80;
pub const SET_FOV: u32 = 0x041C_2D50;
pub const UPDATE_VIEW: u32 = 0x041B_A830;
pub const REFRESH_OUTPUT: u32 = 0x041B_6BB0;
pub const MANAGER_UPDATE: u32 = 0x0121_7150;
pub const RENDERER_GLOBAL: u32 = 0x0AAC_D510;
pub const MANAGER_VTABLE: u32 = 0x0832_ED38;

/// `manager_update` is vtable slot 15 (byte offset 120).
pub const MANAGER_UPDATE_SLOT: usize = 15;

pub const MANAGER_UPDATE_PROLOGUE: &[u8] = &[0x48, 0x8B, 0xC4, 0x48, 0x89, 0x58, 0x10];
pub const CREATE_PROLOGUE: &[u8] = &[0x48, 0x89, 0x5C, 0x24, 0x08];

/// Renderer holds a pointer to the eight scene views here.
pub const RENDERER_VIEW_ARRAY: u64 = 2752;
pub const VIEW_SLOTS: usize = 8;

/// Entry hash table at `manager+88`: count, bucket count, head pointer.
pub const ENTRY_TABLE: u64 = 88;
pub const ENTRY_READY: u64 = 8;
pub const ENTRY_MODE: u64 = 24;
pub const ENTRY_VIEW_INDEX: u64 = 76;
pub const ENTRY_NODE_HANDLE: u64 = 96;
pub const ENTRY_NEXT: u64 = 296;

pub const VIEW_DIMENSIONS: u64 = 16;
pub const VIEW_FLAGS: u64 = 48;
pub const VIEW_ASSOCIATION: u64 = 72;
pub const VIEW_NODE_HANDLE: u64 = 104;
/// `refresh_output` is accepted when it returns this address (`view+144`).
pub const VIEW_OUTPUT: u64 = 144;

pub const NODE_POSITION: u64 = 112;
pub const NODE_CAMERA: u64 = 256;
/// First child, next sibling, parent. Used only after the camera node's own
/// world-matrix origin agrees with `NODE_POSITION`.
pub const NODE_FIRST_CHILD: u64 = 0x110;
pub const NODE_NEXT_SIBLING: u64 = 0x118;
pub const NODE_PARENT: u64 = 0x170;
/// Pointer to four rows (negated right, up, forward, origin). Each row is
/// three f64 at a 32-byte stride.
pub const NODE_WORLD_MATRIX: u64 = 0x128;

pub const CAMERA_TYPE: u64 = 160;
pub const CAMERA_TYPE_VALUE: u16 = 7;
/// `set_fov` stores here (`movss [rcx+0x650], xmm1`). The driver calls the setter.
#[allow(dead_code)]
pub const CAMERA_FOV: u64 = 0x650;
pub const CAMERA_TARGET: u64 = 0x7A0;
pub const CAMERA_UP: u64 = 0x7B8;

/// Near plane. `update_view` loads it (`movss xmm2, [camera+0x5F0]`).
pub const CAMERA_NEAR: u64 = 0x5F0;
/// Culling far. `update_view` loads it (`movss xmm0, [camera+0x5F4]`).
pub const CAMERA_FAR: u64 = 0x5F4;
/// Default far. `update_view` loads it (`movss xmm3, [camera+0x5F8]`).
/// SetFar at `0x041C2D30` writes this and `CAMERA_FAR` from the same float.
/// A view created through `create_entry` comes up at 1000 m, which drops the
/// ground once the slant range exceeds that.
pub const CAMERA_DEFAULT_FAR: u64 = 0x5F8;

/// Independent-pose mode. Skips the engine's built-in pose branches so the
/// setters below own the aim.
pub const MODE_INDEPENDENT: u32 = 2;

/// Nose picture size. Distinct from the main window and from the square shadow map.
pub const NOSE_WIDTH: i32 = 1024;
pub const NOSE_HEIGHT: i32 = 576;

/// Metres along the main view's look direction, ahead of that camera.
/// The live tuner carries this as its stock forward offset.
#[allow(dead_code)]
pub const NOSE_FORWARD_M: f64 = 6.0;
/// Vertical field of view, degrees. The setter takes radians.
/// The live tuner carries this as its stock field.
#[allow(dead_code)]
pub const NOSE_FOV_DEG: f32 = 24.0;
