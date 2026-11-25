//! Which display is streamed, and what a session changes and puts back
//! (design doc 4.4 and 4.5).

use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::com;
use crate::display::Display;

pub fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // Fails when started from Explorer, which has no console; that is fine.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

pub fn local_app_data() -> Option<std::path::PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(std::path::PathBuf::from)
    }
}

pub fn become_dpi_aware() {
    if let Err(error) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        warn!("cannot become DPI aware, coordinates may be scaled: {error}");
    }
}

/// A DXGI output, found again by its index when capture starts.
pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
}

/// Packs adapter and output indices into a [`Display::id`].
fn display_id(adapter: u32, output: u32) -> u32 {
    (adapter << 8) | output
}

pub fn find_output(id: u32) -> windows::core::Result<Output> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let adapter = unsafe { factory.EnumAdapters1(id >> 8)? };
    let output = unsafe { adapter.EnumOutputs(id & 0xff)? };
    Ok(Output { adapter, output })
}

/// The primary display: the attached output whose desktop starts at the
/// origin, else the first attached one.
pub fn main_display() -> Option<Display> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut first = None;
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            if let Ok(desc) = unsafe { output.GetDesc() } {
                if desc.AttachedToDesktop.as_bool() {
                    let rect = desc.DesktopCoordinates;
                    let (width, height) = (
                        (rect.right - rect.left).max(1),
                        (rect.bottom - rect.top).max(1),
                    );
                    let display = Display {
                        id: display_id(adapter_index, output_index),
                        origin: (f64::from(rect.left), f64::from(rect.top)),
                        size: (f64::from(width), f64::from(height)),
                        pixels: (width as u32, height as u32),
                    };
                    if rect.left == 0 && rect.top == 0 {
                        return Some(display);
                    }
                    first.get_or_insert(display);
                }
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    first
}

/// Injection works for every window except those of elevated processes,
/// which Windows shields from a non-elevated sender (UIPI).
pub fn input_permitted(_prompt: bool) -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        queried.is_ok() && elevation.TokenIsElevated != 0
    }
}

pub fn lock_screen() {
    info!("locking the screen");
    if let Err(error) = unsafe { LockWorkStation() } {
        warn!("could not lock the screen: {error}");
    }
}

/// Mutes the default playback device for the length of a session and
/// restores it after.
///
/// The original state is read first, so a machine that was already muted
/// stays muted. Loopback capture taps the mix before the endpoint volume,
/// so the stream keeps its sound. The endpoint is looked up again to
/// restore: COM interfaces cannot move between threads, and the session
/// that owns this may.
pub struct SpeakerMute {
    restore: bool,
}

impl SpeakerMute {
    pub fn engage() -> SpeakerMute {
        let muted = default_endpoint_volume().and_then(|endpoint| unsafe {
            if endpoint.GetMute()?.as_bool() {
                return Ok(false);
            }
            endpoint.SetMute(true, std::ptr::null())?;
            Ok(true)
        });
        match muted {
            Ok(true) => {
                info!("speakers muted for the session");
                SpeakerMute { restore: true }
            }
            Ok(false) => SpeakerMute { restore: false },
            Err(error) => {
                warn!("cannot mute the speakers: {error}");
                SpeakerMute { restore: false }
            }
        }
    }
}

impl Drop for SpeakerMute {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        match default_endpoint_volume()
            .and_then(|endpoint| unsafe { endpoint.SetMute(false, std::ptr::null()) })
        {
            Ok(()) => info!("speakers restored"),
            Err(error) => warn!("cannot restore the speakers: {error}"),
        }
    }
}

fn default_endpoint_volume() -> windows::core::Result<IAudioEndpointVolume> {
    com::enter_mta();
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
        device.Activate(CLSCTX_ALL, None)
    }
}
