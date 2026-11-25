//! Interactive desktop switching (`Default` <-> `Winlogon`) for UAC prompts
//! and the Windows lock screen (design doc 4.1).
//!
//! When running in the active physical console session with a `LocalSystem`
//! token, calling `OpenInputDesktop` + `SetThreadDesktop` allows both the DXGI
//! capture thread and the `SendInput` thread to follow desktop transitions into
//! secure desktops (`Winlogon`) and back to the user desktop (`Default`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::{debug, info};
use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BUSY, GENERIC_ALL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

/// Result of attempting to bind the current thread to the active input desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// Already attached to the active input desktop.
    Unchanged,
    /// Switched to a new desktop (returns its name, e.g. `"Default"` or `"Winlogon"`).
    Switched(String),
    /// `SetThreadDesktop` failed with `ERROR_BUSY` (this thread owns a window or
    /// hook, e.g. created internally by a driver/MFT; caller should respawn the thread).
    Busy,
    /// Desktop could not be opened or switched (e.g. non-privileged token on Winlogon).
    Unavailable,
}

/// Ensures the calling thread is bound to the currently active input desktop.
///
/// Safe to call repeatedly on threads that do not own windows or hooks (the
/// input thread and the DXGI video thread).
pub fn sync_thread_to_input_desktop() -> SwitchOutcome {
    let input = match unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
    } {
        Ok(desk) => desk,
        Err(error) => {
            debug!("OpenInputDesktop failed: {error}");
            return SwitchOutcome::Unavailable;
        }
    };
    let input_name = desktop_name(input);
    let current_name = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
        .ok()
        .and_then(desktop_name);

    if input_name.is_some() && input_name == current_name {
        let _ = unsafe { CloseDesktop(input) };
        return SwitchOutcome::Unchanged;
    }

    match unsafe { SetThreadDesktop(input) } {
        Ok(()) => {
            let name = input_name.unwrap_or_else(|| "unknown".into());
            // The thread now owns a reference to `input`; closing our handle
            // releases only the caller handle without detaching the thread.
            let _ = unsafe { CloseDesktop(input) };
            SwitchOutcome::Switched(name)
        }
        Err(error) => {
            let _ = unsafe { CloseDesktop(input) };
            if error.code() == ERROR_BUSY.to_hresult() {
                debug!("SetThreadDesktop returned ERROR_BUSY (thread owns a window/hook)");
                SwitchOutcome::Busy
            } else {
                debug!("SetThreadDesktop failed: {error}");
                SwitchOutcome::Unavailable
            }
        }
    }
}

/// Reads the name of the currently active input desktop (e.g. `"Default"`, `"Winlogon"`).
pub fn active_input_desktop_name() -> Option<String> {
    let desk = unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
    }
    .ok()?;
    let name = desktop_name(desk);
    let _ = unsafe { CloseDesktop(desk) };
    name
}

fn desktop_name(desk: HDESK) -> Option<String> {
    let mut needed = 0u32;
    let _ =
        unsafe { GetUserObjectInformationW(HANDLE(desk.0), UOI_NAME, None, 0, Some(&mut needed)) };
    if needed == 0 || needed > 4096 {
        return None;
    }
    let mut buf = vec![0u16; (needed as usize).div_ceil(2)];
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut c_void),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Process-wide desktop switch counter bumped by [`DesktopWatcher`] so threads
/// such as input injection can detect transitions with a single atomic load.
static DESKTOP_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn desktop_generation() -> u64 {
    DESKTOP_GENERATION.load(Ordering::Acquire)
}

/// Background thread that detects `Default` <-> `Winlogon` switches using the
/// kernel `WinSta0_DesktopSwitch` event (with a short polling fallback) and
/// bumps [`DESKTOP_GENERATION`] so capture and input threads react immediately.
pub struct DesktopWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DesktopWatcher {
    pub fn start<F>(on_switch: F) -> DesktopWatcher
    where
        F: Fn(String) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = stop.clone();
        let thread = std::thread::Builder::new()
            .name("waystone-desktop-watch".into())
            .spawn(move || watch_loop(stop_bg, on_switch))
            .ok();
        DesktopWatcher { stop, thread }
    }
}

impl Drop for DesktopWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_loop<F: Fn(String)>(stop: Arc<AtomicBool>, on_switch: F) {
    let event = unsafe {
        OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            w!("WinSta0_DesktopSwitch"),
        )
    }
    .ok();
    let mut last_name = active_input_desktop_name();
    if let Some(ref name) = last_name {
        info!("initial input desktop: {name}");
    }

    while !stop.load(Ordering::Relaxed) {
        let signaled = match event {
            Some(handle) => {
                let wait = unsafe { WaitForSingleObject(handle, 100) };
                wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT
            }
            None => {
                std::thread::sleep(Duration::from_millis(100));
                true
            }
        };
        if !signaled || stop.load(Ordering::Relaxed) {
            continue;
        }
        let current = active_input_desktop_name();
        if current != last_name {
            let label = current.clone().unwrap_or_else(|| "unknown".to_string());
            info!("input desktop switched: {:?} -> {}", last_name, label);
            last_name = current;
            DESKTOP_GENERATION.fetch_add(1, Ordering::Release);
            on_switch(label);
            // Brief settle so WinSta0_DesktopSwitch is not re-observed in a tight spin.
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    if let Some(handle) = event {
        let _ = unsafe { CloseHandle(handle) };
    }
}
