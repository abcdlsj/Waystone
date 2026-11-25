//! COM and Media Foundation setup shared by the threads that use them.

use std::sync::OnceLock;

use windows::core::HRESULT;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Joins the calling thread to the multithreaded apartment. Idempotent; a
/// thread that is already in an apartment keeps it.
pub fn enter_mta() {
    // S_FALSE (already initialized) and RPC_E_CHANGED_MODE (an STA thread)
    // both leave COM usable, so the result is not interesting.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Starts Media Foundation once per process; it is never shut down.
pub fn start_media_foundation() -> windows::core::Result<()> {
    static RESULT: OnceLock<HRESULT> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            enter_mta();
            match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
                Ok(()) => HRESULT(0),
                Err(error) => error.code(),
            }
        })
        .ok()
}
