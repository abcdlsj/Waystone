//! Spawning processes into the active physical console session from Session 0.
//!
//! The Session 0 service (`LocalSystem`) has no interactive desktop, display
//! outputs, or audio endpoints. To capture DXGI, record WASAPI loopback, and
//! inject input into elevated windows, UAC prompts, and the Winlogon lock
//! screen, the service duplicates its own `LocalSystem` token, rebinds it to
//! `WTSGetActiveConsoleSessionId()`, enables `TokenUIAccess`, and spawns the
//! worker via `CreateProcessAsUserW`.

use std::ffi::c_void;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, info, warn};
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    TokenUIAccess, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTUPINFOW,
};

use super::security::enable_privileges;

/// Returns the session ID of the physical console, or `None` when none is attached.
pub fn active_console_session_id() -> Option<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == u32::MAX {
        None
    } else {
        Some(id)
    }
}

/// Owned handle to a spawned console-session child process.
pub struct ChildProcess {
    process: HANDLE,
    pid: u32,
    session_id: u32,
}

unsafe impl Send for ChildProcess {}
unsafe impl Sync for ChildProcess {}

impl ChildProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    pub fn is_running(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) }.is_ok()
            && code == STILL_ACTIVE.0 as u32
    }

    /// Waits up to `timeout` for graceful exit; returns `true` if exited.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        unsafe { WaitForSingleObject(self.process, ms) == WAIT_OBJECT_0 }
    }

    pub fn terminate(&self) {
        if self.is_running() {
            let _ = unsafe { TerminateProcess(self.process, 1) };
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if !self.process.is_invalid() {
            let _ = unsafe { CloseHandle(self.process) };
        }
    }
}

/// Spawns `exe` with `args` inside `session_id` as a `LocalSystem` process with
/// `TokenUIAccess = 1`, enabling full access to elevated windows, UAC and Winlogon.
pub fn spawn_privileged_worker(session_id: u32, exe: &Path, args: &str) -> Result<ChildProcess> {
    enable_privileges(&[
        w!("SeAssignPrimaryTokenPrivilege"),
        w!("SeIncreaseQuotaPrivilege"),
        w!("SeTcbPrivilege"),
        w!("SeDebugPrivilege"),
    ]);

    let token = duplicate_system_token_for_session(session_id)?;
    // Prefer the logged-in user's environment when available, falling back to
    // the system token's environment on the pre-login lock screen.
    let env_token = query_user_token(session_id);
    let result = spawn_with_token(token, env_token.unwrap_or(token), session_id, exe, args);
    if let Some(user_token) = env_token {
        let _ = unsafe { CloseHandle(user_token) };
    }
    let _ = unsafe { CloseHandle(token) };
    result
}

/// Fallback lock-screen launcher when the console worker is no longer alive:
/// launches `waystone-host.exe lock-workstation` inside the logged-in user's
/// console session so `LockWorkStation()` runs on the interactive workstation.
pub fn lock_console_session_fallback(exe: &Path) -> Result<()> {
    let Some(session_id) = active_console_session_id() else {
        bail!("no active console session to lock");
    };
    let Some(user_token) = query_user_token(session_id) else {
        // No user is logged into the console (already at pre-login lock screen).
        info!("console session {session_id} has no logged-in user; already locked");
        return Ok(());
    };
    let mut primary = HANDLE::default();
    let dup = unsafe {
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    let _ = unsafe { CloseHandle(user_token) };
    dup.context("duplicating console user token for lock")?;

    let child = spawn_with_token(primary, primary, session_id, exe, "lock-workstation");
    let _ = unsafe { CloseHandle(primary) };
    let child = child?;
    let _ = child.wait_timeout(Duration::from_secs(3));
    Ok(())
}

fn query_user_token(session_id: u32) -> Option<HANDLE> {
    let mut token = HANDLE::default();
    match unsafe { WTSQueryUserToken(session_id, &mut token) } {
        Ok(()) => Some(token),
        Err(error) => {
            debug!("WTSQueryUserToken(session={session_id}) unavailable: {error}");
            None
        }
    }
}

fn duplicate_system_token_for_session(session_id: u32) -> Result<HANDLE> {
    unsafe {
        let mut current = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut current)
            .context("opening service process token")?;

        let mut dup = HANDLE::default();
        let dup_res = DuplicateTokenEx(
            current,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        );
        let _ = CloseHandle(current);
        dup_res.context("duplicating LocalSystem primary token")?;

        if let Err(error) = SetTokenInformation(
            dup,
            TokenSessionId,
            &session_id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        ) {
            let _ = CloseHandle(dup);
            return Err(anyhow!(
                "SetTokenInformation(TokenSessionId={session_id}) failed: {error}"
            ));
        }

        // Enable UIAccess on the duplicated SYSTEM token so SendInput can reach
        // elevated UIAccess windows (e.g. Task Manager, OSK, UAC) without UIPI blocks.
        let ui_access: u32 = 1;
        if let Err(error) = SetTokenInformation(
            dup,
            TokenUIAccess,
            &ui_access as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        ) {
            warn!("SetTokenInformation(TokenUIAccess=1) failed (continuing as SYSTEM): {error}");
        }

        Ok(dup)
    }
}

fn spawn_with_token(
    token: HANDLE,
    env_token: HANDLE,
    session_id: u32,
    exe: &Path,
    args: &str,
) -> Result<ChildProcess> {
    let mut cmdline: Vec<u16> = format!("\"{}\" {}", exe.display(), args)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut desktop: Vec<u16> = "winsta0\\default"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut env: *mut c_void = std::ptr::null_mut();
    let has_env = unsafe { CreateEnvironmentBlock(&mut env, Some(env_token), false) }.is_ok();
    let flags = CREATE_NO_WINDOW
        | if has_env {
            CREATE_UNICODE_ENVIRONMENT
        } else {
            Default::default()
        };

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut proc_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            has_env.then_some(env as *const c_void),
            None,
            &startup,
            &mut proc_info,
        )
    };
    if has_env {
        let _ = unsafe { DestroyEnvironmentBlock(env) };
    }
    created.with_context(|| {
        format!(
            "CreateProcessAsUserW failed in console session {session_id} for {}",
            exe.display()
        )
    })?;
    if !proc_info.hThread.is_invalid() {
        let _ = unsafe { CloseHandle(proc_info.hThread) };
    }
    info!(
        "spawned worker pid={} in console session {}",
        proc_info.dwProcessId, session_id
    );
    Ok(ChildProcess {
        process: proc_info.hProcess,
        pid: proc_info.dwProcessId,
        session_id,
    })
}
