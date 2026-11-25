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
/// so the stream keeps its sound.
pub struct SpeakerMute {
    endpoint: Option<IAudioEndpointVolume>,
}

impl SpeakerMute {
    pub fn engage() -> SpeakerMute {
        let endpoint = match default_endpoint_volume() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                warn!("cannot reach the speakers to mute them: {error}");
                return SpeakerMute { endpoint: None };
            }
        };
        let muted = unsafe { endpoint.GetMute() }.map(|value| value.as_bool());
        let endpoint = match muted {
            Ok(false) => match unsafe { endpoint.SetMute(true, std::ptr::null()) } {
                Ok(()) => {
                    info!("speakers muted for the session");
                    Some(endpoint)
                }
                Err(error) => {
                    warn!("cannot mute the speakers: {error}");
                    None
                }
            },
            _ => None,
        };
        SpeakerMute { endpoint }
    }
}

impl Drop for SpeakerMute {
    fn drop(&mut self) {
        if let Some(endpoint) = self.endpoint.take() {
            com::enter_mta();
            if unsafe { endpoint.SetMute(false, std::ptr::null()) }.is_ok() {
                info!("speakers restored");
            }
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
