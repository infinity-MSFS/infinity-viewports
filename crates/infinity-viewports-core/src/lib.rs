mod contract;
mod control;
mod drive;
mod mount;
mod pose;

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, FreeLibraryAndExitThread, GetModuleFileNameW, GetModuleHandleW,
};
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateThread, GetCurrentProcessId, INFINITE, THREAD_CREATION_FLAGS,
    WaitForSingleObject,
};
use windows::core::PCWSTR;

use contract::{MANAGER_UPDATE, MANAGER_UPDATE_SLOT, MANAGER_VTABLE, NOSE_HEIGHT, NOSE_WIDTH};
use drive::Driver;

static MODULE: AtomicUsize = AtomicUsize::new(0);
static IN_HOOK: AtomicBool = AtomicBool::new(false);
static DRIVER: Mutex<Option<Driver>> = Mutex::new(None);
static LAST_LOG: Mutex<String> = Mutex::new(String::new());

std::arch::global_asm!(
    r#"
        .globl nose_update_thunk
        .p2align 4
        nose_update_thunk:
            push rbp
            pushfq
            sub rsp, 232
            mov [rsp + 32], rax
            mov [rsp + 40], rcx
            mov [rsp + 48], rdx
            mov [rsp + 56], r8
            mov [rsp + 64], r9
            mov [rsp + 72], r10
            mov [rsp + 80], r11
            movdqu [rsp + 96], xmm0
            movdqu [rsp + 112], xmm1
            movdqu [rsp + 128], xmm2
            movdqu [rsp + 144], xmm3
            movdqu [rsp + 160], xmm4
            movdqu [rsp + 176], xmm5
            stmxcsr [rsp + 192]
            mov rcx, [rsp + 40]
            call {step}
            ldmxcsr [rsp + 192]
            movdqu xmm0, [rsp + 96]
            movdqu xmm1, [rsp + 112]
            movdqu xmm2, [rsp + 128]
            movdqu xmm3, [rsp + 144]
            movdqu xmm4, [rsp + 160]
            movdqu xmm5, [rsp + 176]
            mov rax, [rsp + 32]
            mov rcx, [rsp + 40]
            mov rdx, [rsp + 48]
            mov r8, [rsp + 56]
            mov r9, [rsp + 64]
            mov r10, [rsp + 72]
            mov r11, [rsp + 80]
            add rsp, 232
            popfq
            pop rbp
            jmp qword ptr [rip + {original}]
    "#,
    step = sym nose_on_update,
    original = sym NOSE_UPDATE_ORIGINAL,
);

unsafe extern "C" {
    fn nose_update_thunk();
}

static mut NOSE_UPDATE_ORIGINAL: u64 = 0;

unsafe extern "C" fn nose_on_update(manager: *mut c_void) {
    if IN_HOOK.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut guard = DRIVER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(driver) = guard.as_mut() else {
            return;
        };
        let before = driver.phase_name();
        let result = driver.step(manager);
        let after = driver.phase_name();
        if let Some(note) = driver.take_note() {
            log_changed(&note);
        }
        if before != after {
            log_changed(&format!(
                "phase {before} -> {after} ({NOSE_WIDTH}x{NOSE_HEIGHT})"
            ));
        }
        if let Err(err) = result {
            log_changed(&format!("error: {err}"));
        }
    }));
    IN_HOOK.store(false, Ordering::Release);
}

fn install() {
    let base = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module.0 as usize,
        Err(err) => {
            log_line(&format!("GetModuleHandleW(exe) failed: {err}"));
            return;
        }
    };
    if let Err(err) = Driver::installable(base) {
        log_line(&format!("refusing to hook: {err}"));
        return;
    }
    let slot =
        (base + MANAGER_VTABLE as usize + MANAGER_UPDATE_SLOT * size_of::<usize>()) as *mut usize;
    let expected = base + MANAGER_UPDATE as usize;
    unsafe {
        NOSE_UPDATE_ORIGINAL = expected as u64;
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if let Err(err) = VirtualProtect(
            slot.cast(),
            size_of::<usize>(),
            PAGE_EXECUTE_READWRITE,
            &mut old,
        ) {
            log_line(&format!("VirtualProtect failed: {err}"));
            return;
        }
        let atomic = AtomicUsize::from_ptr(slot);
        let swapped = atomic.compare_exchange(
            expected,
            nose_update_thunk as *const () as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        let mut restored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut restored);
        match swapped {
            Ok(_) => {
                match DRIVER.lock() {
                    Ok(mut guard) => *guard = Some(Driver::new(base)),
                    Err(poisoned) => *poisoned.into_inner() = Some(Driver::new(base)),
                }
                log_line(&format!(
                    "hooked manager update at {expected:#x}; nose target {NOSE_WIDTH}x{NOSE_HEIGHT}"
                ));
                if let Ok(thread) = CreateThread(
                    None,
                    0,
                    Some(teardown_watcher),
                    None,
                    THREAD_CREATION_FLAGS(0),
                    None,
                ) {
                    let _ = CloseHandle(thread);
                }
            }
            Err(found) => log_line(&format!(
                "vtable slot changed to {found:#x} before the swap; not hooked"
            )),
        }
    }
}

fn teardown_event_name(pid: u32) -> String {
    format!("Local\\InfinityViewports.{pid}.teardown")
}

fn unhook() -> bool {
    let base = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module.0 as usize,
        Err(_) => return false,
    };
    let original = unsafe { core::ptr::addr_of!(NOSE_UPDATE_ORIGINAL).read() } as usize;
    if original == 0 {
        return false;
    }
    let slot =
        (base + MANAGER_VTABLE as usize + MANAGER_UPDATE_SLOT * size_of::<usize>()) as *mut usize;
    let thunk = nose_update_thunk as *const () as usize;
    let restored = unsafe {
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(
            slot.cast(),
            size_of::<usize>(),
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
        .is_err()
        {
            return false;
        }
        let atomic = AtomicUsize::from_ptr(slot);
        let ok = atomic
            .compare_exchange(thunk, original, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        let mut back = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut back);
        ok
    };
    match DRIVER.lock() {
        Ok(mut g) => *g = None,
        Err(p) => *p.into_inner() = None,
    }
    restored
}

unsafe extern "system" fn teardown_watcher(_: *mut c_void) -> u32 {
    let pid = unsafe { GetCurrentProcessId() };
    let name: Vec<u16> = teardown_event_name(pid)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let Ok(event) = (unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }) else {
        log_line("teardown watcher: CreateEventW failed; teardown unavailable");
        return 0;
    };
    if unsafe { WaitForSingleObject(event, INFINITE) }.0 != 0 {
        let _ = unsafe { CloseHandle(event) };
        return 0;
    }
    let _ = unsafe { CloseHandle(event) };
    log_line("teardown signal received; unhooking");
    let restored = unhook();
    log_line(&format!(
        "unhook: slot restored={restored}; draining then unloading infinity_viewports_core"
    ));
    std::thread::sleep(Duration::from_millis(250));
    let module = MODULE.load(Ordering::Acquire);
    unsafe { FreeLibraryAndExitThread(HMODULE(module as *mut c_void), 0) }
}

fn log_changed(msg: &str) {
    let mut last = LAST_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *last == msg {
        return;
    }
    *last = msg.to_string();
    drop(last);
    log_line(msg);
}

fn log_line(msg: &str) {
    let Some(path) = log_path() else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{msg}");
}

fn log_path() -> Option<PathBuf> {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(path) = PATH.get() {
        return Some(path.clone());
    }
    let module = MODULE.load(Ordering::Acquire);
    if module == 0 {
        return None;
    }
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(HMODULE(module as *mut c_void)), &mut buf) };
    if n == 0 {
        return None;
    }
    let mut path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]));
    path.set_file_name("infinity_viewports_core.log");
    let _ = PATH.set(path.clone());
    Some(path)
}

unsafe extern "system" fn boot(_: *mut c_void) -> u32 {
    install();
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    module: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module as usize, Ordering::Release);
        unsafe {
            let _ = DisableThreadLibraryCalls(HMODULE(module));
            if let Ok(thread) =
                CreateThread(None, 0, Some(boot), None, THREAD_CREATION_FLAGS(0), None)
            {
                let _ = CloseHandle(thread);
            }
        }
    }
    1
}
