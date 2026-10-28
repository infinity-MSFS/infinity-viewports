#![cfg_attr(not(all(windows, target_env = "msvc")), allow(unused))]

#[cfg(all(windows, target_env = "msvc"))]
mod ffi {
    use core::ffi::c_void;
    unsafe extern "C" {
        pub fn infinity_viewports_guarded_read(
            dst: *mut c_void,
            src: *const c_void,
            len: usize,
        ) -> i32;
        pub fn infinity_viewports_guarded_read_ptr(
            src: *const c_void,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn infinity_viewports_guarded_call2(
            fn_: extern "C" fn(*mut c_void, *mut c_void),
            rcx: *mut c_void,
            rdx: *mut c_void,
        ) -> i32;
        pub fn infinity_viewports_guarded_call2_ret(
            fn_: extern "C" fn(*mut c_void, *mut c_void) -> u64,
            rcx: *mut c_void,
            rdx: *mut c_void,
            out: *mut u64,
        ) -> i32;
        pub fn infinity_viewports_guarded_write(
            dst: *mut c_void,
            src: *const c_void,
            len: usize,
        ) -> i32;
        pub fn infinity_viewports_guarded_call1(
            fn_: extern "C" fn(*mut c_void),
            rcx: *mut c_void,
        ) -> i32;
        pub fn infinity_viewports_guarded_call1_ret(
            fn_: extern "C" fn(*mut c_void) -> *mut c_void,
            rcx: *mut c_void,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn infinity_viewports_guarded_activate(
            fn_: extern "C" fn(*mut c_void, u64, i32),
            mgr: *mut c_void,
            id: u64,
            open: i32,
        ) -> i32;
        pub fn infinity_viewports_guarded_set_fov(
            fn_: extern "C" fn(*mut c_void, f32),
            camera: *mut c_void,
            fov: f32,
        ) -> i32;
        pub fn infinity_viewports_guarded_attach(
            fn_: extern "C" fn(*mut c_void, *const c_void, i32),
            parent: *mut c_void,
            handle: *const c_void,
            keep_world: i32,
        ) -> i32;
        pub fn infinity_viewports_guarded_detach(
            fn_: extern "C" fn(*mut c_void, i32, i32),
            node: *mut c_void,
            keep_children: i32,
            keep_world: i32,
        ) -> i32;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Faulted;

pub type Guarded<T> = Result<T, Faulted>;

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_read(dst: &mut [u8], src: *const u8) -> Guarded<()> {
    let ok = unsafe {
        ffi::infinity_viewports_guarded_read(dst.as_mut_ptr().cast(), src.cast(), dst.len())
    };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_read_ptr(src: *const u8) -> Guarded<usize> {
    let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
    let ok = unsafe { ffi::infinity_viewports_guarded_read_ptr(src.cast(), &mut out) };
    if ok != 0 {
        Ok(out as usize)
    } else {
        Err(Faulted)
    }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_call2(
    fn_: extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void),
    rcx: *mut core::ffi::c_void,
    rdx: *mut core::ffi::c_void,
) -> Guarded<()> {
    let ok = unsafe { ffi::infinity_viewports_guarded_call2(fn_, rcx, rdx) };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_call2_ret(
    fn_: extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> u64,
    rcx: *mut core::ffi::c_void,
    rdx: *mut core::ffi::c_void,
) -> Guarded<u64> {
    let mut out = 0u64;
    let ok = unsafe { ffi::infinity_viewports_guarded_call2_ret(fn_, rcx, rdx, &mut out) };
    if ok != 0 { Ok(out) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_write(dst: *mut u8, src: &[u8]) -> Guarded<()> {
    let ok = unsafe {
        ffi::infinity_viewports_guarded_write(dst.cast(), src.as_ptr().cast(), src.len())
    };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_call1(
    fn_: extern "C" fn(*mut core::ffi::c_void),
    rcx: *mut core::ffi::c_void,
) -> Guarded<()> {
    let ok = unsafe { ffi::infinity_viewports_guarded_call1(fn_, rcx) };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_call1_ret(
    fn_: extern "C" fn(*mut core::ffi::c_void) -> *mut core::ffi::c_void,
    rcx: *mut core::ffi::c_void,
) -> Guarded<*mut core::ffi::c_void> {
    let mut out = core::ptr::null_mut();
    let ok = unsafe { ffi::infinity_viewports_guarded_call1_ret(fn_, rcx, &mut out) };
    if ok != 0 { Ok(out) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_activate(
    fn_: extern "C" fn(*mut core::ffi::c_void, u64, i32),
    mgr: *mut core::ffi::c_void,
    id: u64,
    open: bool,
) -> Guarded<()> {
    let ok = unsafe { ffi::infinity_viewports_guarded_activate(fn_, mgr, id, i32::from(open)) };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_set_fov(
    fn_: extern "C" fn(*mut core::ffi::c_void, f32),
    camera: *mut core::ffi::c_void,
    fov: f32,
) -> Guarded<()> {
    let ok = unsafe { ffi::infinity_viewports_guarded_set_fov(fn_, camera, fov) };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_attach(
    fn_: extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, i32),
    parent: *mut core::ffi::c_void,
    handle: *const core::ffi::c_void,
    keep_world: bool,
) -> Guarded<()> {
    let ok = unsafe {
        ffi::infinity_viewports_guarded_attach(fn_, parent, handle, i32::from(keep_world))
    };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(all(windows, target_env = "msvc"))]
pub unsafe fn guarded_detach(
    fn_: extern "C" fn(*mut core::ffi::c_void, i32, i32),
    node: *mut core::ffi::c_void,
    keep_children: bool,
    keep_world: bool,
) -> Guarded<()> {
    let ok = unsafe {
        ffi::infinity_viewports_guarded_detach(
            fn_,
            node,
            i32::from(keep_children),
            i32::from(keep_world),
        )
    };
    if ok != 0 { Ok(()) } else { Err(Faulted) }
}

#[cfg(not(all(windows, target_env = "msvc")))]
mod fallback {
    use super::{Faulted, Guarded};
    use core::ffi::c_void;

    pub unsafe fn guarded_read(_dst: &mut [u8], _src: *const u8) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_read_ptr(_src: *const u8) -> Guarded<usize> {
        Err(Faulted)
    }
    pub unsafe fn guarded_call2(
        _fn: extern "C" fn(*mut c_void, *mut c_void),
        _rcx: *mut c_void,
        _rdx: *mut c_void,
    ) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_call2_ret(
        _fn: extern "C" fn(*mut c_void, *mut c_void) -> u64,
        _rcx: *mut c_void,
        _rdx: *mut c_void,
    ) -> Guarded<u64> {
        Err(Faulted)
    }
    pub unsafe fn guarded_write(_dst: *mut u8, _src: &[u8]) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_call1(_fn: extern "C" fn(*mut c_void), _rcx: *mut c_void) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_call1_ret(
        _fn: extern "C" fn(*mut c_void) -> *mut c_void,
        _rcx: *mut c_void,
    ) -> Guarded<*mut c_void> {
        Err(Faulted)
    }
    pub unsafe fn guarded_activate(
        _fn: extern "C" fn(*mut c_void, u64, i32),
        _mgr: *mut c_void,
        _id: u64,
        _open: bool,
    ) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_set_fov(
        _fn: extern "C" fn(*mut c_void, f32),
        _camera: *mut c_void,
        _fov: f32,
    ) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_attach(
        _fn: extern "C" fn(*mut c_void, *const c_void, i32),
        _parent: *mut c_void,
        _handle: *const c_void,
        _keep_world: bool,
    ) -> Guarded<()> {
        Err(Faulted)
    }
    pub unsafe fn guarded_detach(
        _fn: extern "C" fn(*mut c_void, i32, i32),
        _node: *mut c_void,
        _keep_children: bool,
        _keep_world: bool,
    ) -> Guarded<()> {
        Err(Faulted)
    }
}

#[cfg(not(all(windows, target_env = "msvc")))]
pub use fallback::*;
