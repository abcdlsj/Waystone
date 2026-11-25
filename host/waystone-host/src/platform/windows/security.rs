//! Windows security descriptors, DACL enforcement and token privilege helpers.
//!
//! * The per-session worker Named Pipe and `device.key` are restricted to
//!   `LocalSystem` (`SY`) and `BUILTIN\Administrators` (`BA`) only.
//! * The tray/CLI control Named Pipe additionally grants read/write to
//!   `INTERACTIVE` (`IU`) so a non-elevated taskbar tray on the local console
//!   can inspect status and steer the service without allowing remote users.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tracing::debug;
use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL, LUID};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetSecurityDescriptorDacl, LookupPrivilegeValueW, ACL,
    DACL_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Protected DACL allowing only `LocalSystem` and `BUILTIN\Administrators`.
pub const WORKER_PIPE_SDDL: PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)");

/// Protected DACL allowing `LocalSystem`, `BUILTIN\Administrators`, and locally
/// logged-in interactive users (`IU`) to communicate with the service control pipe.
pub const CONTROL_PIPE_SDDL: PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

/// Protected DACL for sensitive files (`device.key`) managed by the service.
pub const PRIVATE_FILE_SDDL: PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)");

/// Owned Win32 security descriptor allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
pub struct SecurityDescriptor {
    raw: PSECURITY_DESCRIPTOR,
}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: PCWSTR) -> Result<SecurityDescriptor> {
        let mut raw = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &mut raw,
                None,
            )
        }
        .context("parsing SDDL security descriptor")?;
        if raw.0.is_null() {
            return Err(anyhow!("empty security descriptor"));
        }
        Ok(SecurityDescriptor { raw })
    }

    /// Builds a `SECURITY_ATTRIBUTES` pointing into this descriptor.
    /// The returned struct must not outlive `self`.
    pub fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.raw.0,
            bInheritHandle: BOOL(0),
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.raw.0)));
            }
        }
    }
}

/// Applies a protected DACL restricting `path` to `LocalSystem` and
/// `BUILTIN\Administrators` only.
pub fn protect_file_for_system_and_admins(path: &Path) -> Result<()> {
    let sd = SecurityDescriptor::from_sddl(PRIVATE_FILE_SDDL)?;
    let mut dacl_present = BOOL(0);
    let mut dacl_defaulted = BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(sd.raw, &mut dacl_present, &mut dacl, &mut dacl_defaulted)
            .context("extracting DACL from security descriptor")?;
    }
    if !dacl_present.as_bool() || dacl.is_null() {
        return Err(anyhow!("SDDL did not yield a DACL"));
    }
    let wide = HSTRING::from(path.as_os_str());
    let err = unsafe {
        SetNamedSecurityInfoW(
            &wide,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(anyhow!(
            "SetNamedSecurityInfoW failed on {}: {:?}",
            path.display(),
            err
        ));
    }
    Ok(())
}

/// Best-effort enables the named privileges on the current process token
/// (required before `SetTokenInformation(..., TokenSessionId, ...)` and
/// `CreateProcessAsUserW` from Session 0).
pub fn enable_privileges(names: &[PCWSTR]) {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return;
        }
        for &name in names {
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid).is_err() {
                continue;
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            if let Err(error) = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None) {
                debug!("AdjustTokenPrivileges failed: {error}");
            }
        }
        let _ = CloseHandle(token);
    }
}
