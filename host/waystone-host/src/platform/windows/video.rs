//! The video half of a capture session, all on one thread and one GPU:
//! desktop duplication → a BGRA canvas with the cursor drawn in → the video
//! processor (scale and BGRA to NV12) → a Media Foundation hardware encoder.
//!
//! No frame ever leaves video memory (design doc 4.3). The encoder runs
//! without B-frames, with an unlimited GOP and keyframes only on request,
//! and when the desktop is still the last picture is re-encoded at 5 fps so
//! a viewer that just joined, or lost a packet, gets a picture.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, info, warn};
use windows::core::{s, w, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{HMODULE, LUID, POINT, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutput5, IDXGIOutput6, IDXGIOutputDuplication, IDXGIResource, IDXGISurface1,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, DI_NORMAL,
    HICON, ICONINFO, IDC_ARROW,
};

use waystone_proto::VideoCodec;

use super::desktop::{self, SwitchOutcome};
use super::system::{find_output, Output};

/// Outcome of one [`Pipeline::step`] turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Continue,
    /// `SetThreadDesktop` returned `ERROR_BUSY` because a driver or Media
    /// Foundation component attached a window/hook to this thread. The caller
    /// should drop the pipeline and start a fresh video thread on the new desktop.
    RespawnThread,
}

/// Re-encode the last picture this often once the desktop is still.
const KEEPALIVE: Duration = Duration::from_millis(200);
/// How long the desktop must be still before keepalive starts.
const IDLE_AFTER: Duration = Duration::from_millis(500);
/// NV12 surfaces in rotation, so the encoder can still read one while the
/// next is written.
const SURFACES: usize = 4;

/// What the session asked for; `bitrate` and `fps` may change while running.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub codec: VideoCodec,
}

/// One encoded access unit.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub micros: u64,
}

/// Everything the video thread owns.
pub struct Pipeline {
    output: Output,
    luid: LUID,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<Duplication>,
    canvas: Option<Canvas>,
    tone_mapper: Option<HdrToneMapper>,
    converter: Option<Converter>,
    encoder: Encoder,
    settings: Settings,
    clock: Clock,
    _timer: TimerPeriod,
    /// A picture is on the canvas (the first frame arrived).
    has_picture: bool,
    /// The canvas changed since it was last encoded.
    dirty: bool,
    /// Latest pointer state reported by DXGI (`(x, y, visible)` in output pixels).
    dxgi_pointer: Option<(i32, i32, bool)>,
    /// Last cursor `(x, y, handle)` composited onto the canvas.
    last_cursor: Option<(i32, i32, usize)>,
    last_change: Instant,
    last_encode: Option<Instant>,
    keyframe_wanted: bool,
}

impl Pipeline {
    pub fn new(display_id: u32, settings: Settings) -> Result<Pipeline> {
        super::com::start_media_foundation().context("starting Media Foundation")?;
        let output = find_output(display_id).context("finding the display")?;
        let luid = unsafe { output.adapter.GetDesc1() }?.AdapterLuid;

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &output.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("creating the Direct3D device")?;
        let device = device.ok_or_else(|| anyhow!("no Direct3D device"))?;
        let context = context.ok_or_else(|| anyhow!("no Direct3D context"))?;
        // The encoder calls into the device from its own threads.
        let _ = unsafe {
            device
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true)
        };

        let encoder = Encoder::new(&device, luid, &settings).context("starting the encoder")?;
        let mut pipeline = Pipeline {
            output,
            luid,
            device,
            context,
            duplication: None,
            canvas: None,
            tone_mapper: None,
            converter: None,
            encoder,
            settings,
            clock: Clock::new(),
            _timer: TimerPeriod::new(1),
            has_picture: false,
            dirty: false,
            dxgi_pointer: None,
            last_cursor: None,
            last_change: Instant::now(),
            last_encode: None,
            keyframe_wanted: true,
        };
        pipeline.reconnect().context(
            "duplicating the desktop; the host must run in the logged-in user's session",
        )?;
        Ok(pipeline)
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe_wanted = true;
        // A still desktop would otherwise wait for the next keepalive.
        self.dirty |= self.has_picture;
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.settings.bitrate {
            self.settings.bitrate = bitrate;
            self.encoder.set_bitrate(bitrate);
        }
    }

    pub fn set_frame_rate(&mut self, fps: u32) {
        if fps != self.settings.fps && fps > 0 {
            self.settings.fps = fps;
            self.encoder.set_frame_rate(fps);
        }
    }

    /// Rebuilds the scaler and encoder for a new stream resolution mid-session.
    pub fn set_resolution(&mut self, width: u32, height: u32, bitrate: u32) -> Result<()> {
        if (width, height) == (self.settings.width, self.settings.height) {
            self.set_bitrate(bitrate);
            return Ok(());
        }
        let next = Settings {
            width,
            height,
            bitrate,
            ..self.settings
        };
        let encoder = Encoder::new(&self.device, self.luid, &next)
            .context("restarting encoder for new resolution")?;
        if let Some(canvas) = &self.canvas {
            self.converter = Some(Converter::new(
                &self.device,
                &self.context,
                &canvas.vp_input,
                canvas.size,
                &next,
            )?);
        }
        self.encoder = encoder;
        self.settings = next;
        self.keyframe_wanted = true;
        self.dirty |= self.has_picture;
        self.last_encode = None;
        info!(
            "stream resolution switched to {}x{}@{} ({} kbit/s)",
            next.width,
            next.height,
            next.fps,
            next.bitrate / 1000
        );
        Ok(())
    }

    /// Releases desktop duplication and re-attaches the thread to the active
    /// input desktop (`Default` <-> `Winlogon`). Returns `false` if the thread
    /// owns a window/hook (`ERROR_BUSY`) and must be respawned.
    pub fn handle_desktop_switch(&mut self) -> bool {
        self.duplication = None;
        match desktop::sync_thread_to_input_desktop() {
            SwitchOutcome::Busy => return false,
            SwitchOutcome::Switched(name) => {
                info!("video thread switched to desktop {name}");
            }
            SwitchOutcome::Unchanged | SwitchOutcome::Unavailable => {}
        }
        if let Err(error) = self.reconnect() {
            debug!("desktop duplication reconnect after switch deferred: {error:#}");
        }
        true
    }

    /// One turn of the loop: collect encoder output, take a desktop frame
    /// if there is one, and encode when a frame is due.
    pub fn step(&mut self, deliver: &mut dyn FnMut(EncodedFrame)) -> Result<StepStatus> {
        self.encoder.poll(deliver)?;

        let timeout_ms = self.acquire_timeout_ms();
        match self.acquire(timeout_ms) {
            Ok(()) => {}
            Err(error) => {
                // Lock screen, UAC prompt, mode change or a fullscreen game
                // switching modes. Keep the last picture, follow any desktop
                // switch, and retry.
                debug!("desktop duplication interrupted: {error:#}");
                self.duplication = None;
                match desktop::sync_thread_to_input_desktop() {
                    SwitchOutcome::Busy => return Ok(StepStatus::RespawnThread),
                    SwitchOutcome::Switched(name) => {
                        info!("video thread switched to desktop {name}");
                    }
                    SwitchOutcome::Unchanged | SwitchOutcome::Unavailable => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                if let Err(error) = self.reconnect() {
                    debug!("desktop duplication unavailable: {error:#}");
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
        }

        // Collect any frame the encoder finished while `acquire` waited so
        // encoded output is never held back by an entire acquire timeout.
        self.encoder.poll(deliver)?;

        let now = Instant::now();
        let interval = Duration::from_secs(1) / self.settings.fps.max(1);
        let since_encode = self.last_encode.map(|at| now - at);
        // A little early is better than a whole interval late.
        let due = since_encode.is_none_or(|elapsed| elapsed + interval / 8 >= interval);
        let idle = now - self.last_change >= IDLE_AFTER;
        let keepalive = idle && since_encode.is_none_or(|elapsed| elapsed >= KEEPALIVE);
        if self.has_picture && self.encoder.ready() && ((self.dirty && due) || keepalive) {
            self.encode(now)?;
            self.encoder.poll(deliver)?;
        }
        Ok(StepStatus::Continue)
    }

    fn acquire_timeout_ms(&self) -> u32 {
        let interval = Duration::from_secs(1) / self.settings.fps.max(1);
        let elapsed = self.last_encode.map(|at| at.elapsed());
        let due_now = elapsed.is_none_or(|e| e + interval / 8 >= interval);
        if self.has_picture && self.dirty && due_now && self.encoder.ready() {
            // A frame is already waiting to encode; grab any immediate desktop
            // update without blocking past the frame deadline.
            return 0;
        }
        if self.encoder.in_flight > 0 {
            // Hardware encoders typically finish in 1-2 ms; poll promptly.
            return 1;
        }
        if self.dirty {
            let remaining = elapsed
                .and_then(|e| interval.checked_sub(e + interval / 8))
                .map(|d| d.as_millis() as u32)
                .unwrap_or(1);
            return remaining.clamp(1, 4);
        }
        4
    }

    fn encode(&mut self, now: Instant) -> Result<()> {
        let converter = self
            .converter
            .as_mut()
            .expect("a picture implies a converter");
        let surface = converter.convert()?;
        let time = self.clock.micros() as i64 * 10;
        self.encoder.encode(&surface, time, self.keyframe_wanted)?;
        self.keyframe_wanted = false;
        self.dirty = false;
        self.last_encode = Some(now);
        Ok(())
    }

    /// Waits briefly for a desktop update and copies it onto the canvas.
    fn acquire(&mut self, timeout_ms: u32) -> Result<()> {
        let Some(duplication) = self.duplication.as_ref().map(|d| d.handle.clone()) else {
            bail!("not duplicating");
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // If the cursor moved without DXGI reporting a mouse update
                // (e.g. injected move on a static desktop), re-composite it.
                self.refresh_cursor_if_moved();
                return Ok(());
            }
            Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => bail!("access lost"),
            Err(error) => return Err(error.into()),
        }
        let result = self.take_frame(&info, resource);
        let released = unsafe { duplication.ReleaseFrame() };
        result?;
        released?;
        Ok(())
    }

    fn take_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<()> {
        if info.LastMouseUpdateTime != 0 {
            self.dxgi_pointer = Some((
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
                info.PointerPosition.Visible.as_bool(),
            ));
        }
        // Neither the image nor the pointer changed.
        if info.LastPresentTime == 0 && info.LastMouseUpdateTime == 0 && self.has_picture {
            return Ok(());
        }
        let image_updated = info.LastPresentTime != 0 || !self.has_picture;
        if image_updated {
            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| anyhow!("frame without a surface"))?
                .cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            let canvas = self.canvas.as_ref().expect("duplication implies a canvas");
            if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let mapper = self
                    .tone_mapper
                    .as_mut()
                    .ok_or_else(|| anyhow!("HDR frame arrived without a tone mapper"))?;
                mapper.apply(
                    &self.device,
                    &self.context,
                    &texture,
                    &canvas.desktop_rtv,
                    canvas.size,
                )?;
            } else {
                unsafe { self.context.CopyResource(&canvas.desktop, &texture) };
            }
            self.has_picture = true;
        }
        self.composite_canvas();
        Ok(())
    }

    fn refresh_cursor_if_moved(&mut self) {
        if !self.has_picture {
            return;
        }
        let current = current_cursor_key();
        if current != self.last_cursor {
            self.composite_canvas();
        }
    }

    /// Copies the clean desktop onto the GDI surface, draws the cursor, and
    /// copies the result into the video processor's input texture.
    fn composite_canvas(&mut self) {
        let origin = self.duplication.as_ref().expect("duplication").origin;
        let canvas = self.canvas.as_mut().expect("duplication implies a canvas");
        unsafe { self.context.CopyResource(&canvas.gdi, &canvas.desktop) };
        self.last_cursor = canvas.draw_cursor(origin, self.dxgi_pointer);
        unsafe { self.context.CopyResource(&canvas.vp_input, &canvas.gdi) };
        self.dirty = true;
        self.last_change = Instant::now();
    }

    /// (Re)creates the duplication, and the canvas and converter when the
    /// desktop size changed.
    fn reconnect(&mut self) -> Result<()> {
        let hdr = self.is_hdr_output();
        let handle = if hdr {
            match self.duplicate_hdr() {
                Ok(handle) => {
                    let sdr_scale = query_sdr_white_scale(&self.output.output, self.luid);
                    match HdrToneMapper::new(&self.device, sdr_scale) {
                        Ok(mapper) => {
                            info!(
                                "HDR desktop active: GPU tone-mapping scRGB to SDR (SDR white {:.0} nits)",
                                sdr_scale * 80.0
                            );
                            self.tone_mapper = Some(mapper);
                            handle
                        }
                        Err(error) => {
                            warn!("HDR tone mapper init failed, falling back to SDR duplication: {error:#}");
                            self.tone_mapper = None;
                            let output: IDXGIOutput1 = self.output.output.cast()?;
                            unsafe { output.DuplicateOutput(&self.device) }?
                        }
                    }
                }
                Err(error) => {
                    warn!(
                        "HDR DuplicateOutput1 failed, falling back to SDR duplication: {error:#}"
                    );
                    self.tone_mapper = None;
                    let output: IDXGIOutput1 = self.output.output.cast()?;
                    unsafe { output.DuplicateOutput(&self.device) }?
                }
            }
        } else {
            self.tone_mapper = None;
            let output: IDXGIOutput1 = self.output.output.cast()?;
            unsafe { output.DuplicateOutput(&self.device) }?
        };
        let desc = unsafe { handle.GetDesc() };
        let size = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        let origin = unsafe { self.output.output.GetDesc() }?.DesktopCoordinates;
        if self.canvas.as_ref().map(|canvas| canvas.size) != Some(size) {
            info!("desktop is {}x{}", size.0, size.1);
            let canvas = Canvas::new(&self.device, size)?;
            self.converter = Some(Converter::new(
                &self.device,
                &self.context,
                &canvas.vp_input,
                size,
                &self.settings,
            )?);
            self.canvas = Some(canvas);
            self.has_picture = false;
            self.last_cursor = None;
        }
        self.duplication = Some(Duplication {
            handle,
            origin: (origin.left, origin.top),
        });
        self.keyframe_wanted = true;
        Ok(())
    }

    fn is_hdr_output(&self) -> bool {
        let Ok(output6) = self.output.output.cast::<IDXGIOutput6>() else {
            return false;
        };
        unsafe { output6.GetDesc1() }
            .map(|desc| desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
            .unwrap_or(false)
    }

    fn duplicate_hdr(&self) -> Result<IDXGIOutputDuplication> {
        let output5: IDXGIOutput5 = self.output.output.cast()?;
        let formats = [DXGI_FORMAT_R16G16B16A16_FLOAT];
        let handle = unsafe { output5.DuplicateOutput1(&self.device, 0, &formats) }?;
        Ok(handle)
    }
}

/// Raises the Windows system timer resolution for the lifetime of a capture
/// session so short DXGI waits and 60 Hz frame pacing do not round to 15.6 ms.
struct TimerPeriod(u32);

impl TimerPeriod {
    fn new(ms: u32) -> TimerPeriod {
        let rc = unsafe { timeBeginPeriod(ms) };
        TimerPeriod(if rc == 0 { ms } else { 0 })
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe { timeEndPeriod(self.0) };
        }
    }
}

struct Duplication {
    handle: IDXGIOutputDuplication,
    /// Desktop coordinates of the output's top left corner.
    origin: (i32, i32),
}

/// The latest desktop image plus a dedicated GDI surface for cursor drawing.
///
/// Keeping `desktop`, `gdi` and `vp_input` separate solves two D3D11/DXGI
/// hazards: pointer-only frames (`LastPresentTime == 0`) do not lose the clean
/// desktop pixels, and `IDXGISurface1::GetDC` never contends with the
/// `ID3D11VideoProcessorInputView` bound to `vp_input`.
struct Canvas {
    desktop: ID3D11Texture2D,
    desktop_rtv: ID3D11RenderTargetView,
    gdi: ID3D11Texture2D,
    vp_input: ID3D11Texture2D,
    size: (u32, u32),
    cursor_logged: bool,
    cursor_error_logged: bool,
}

impl Canvas {
    fn new(device: &ID3D11Device, (width, height): (u32, u32)) -> Result<Canvas> {
        let base_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let gdi_desc = D3D11_TEXTURE2D_DESC {
            MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
            ..base_desc
        };
        let create = |desc: &D3D11_TEXTURE2D_DESC, label: &str| -> Result<ID3D11Texture2D> {
            let mut texture = None;
            unsafe { device.CreateTexture2D(desc, None, Some(&mut texture)) }
                .with_context(|| format!("creating {label}"))?;
            texture.ok_or_else(|| anyhow!("no {label}"))
        };
        let desktop = create(&base_desc, "the desktop texture")?;
        let mut desktop_rtv = None;
        unsafe { device.CreateRenderTargetView(&desktop, None, Some(&mut desktop_rtv)) }
            .context("creating the desktop render target view")?;
        Ok(Canvas {
            desktop,
            desktop_rtv: desktop_rtv.ok_or_else(|| anyhow!("no desktop render target view"))?,
            gdi: create(&gdi_desc, "the GDI cursor texture")?,
            vp_input: create(&base_desc, "the video processor input texture")?,
            size: (width, height),
            cursor_logged: false,
            cursor_error_logged: false,
        })
    }

    fn draw_cursor(
        &mut self,
        origin: (i32, i32),
        dxgi_pointer: Option<(i32, i32, bool)>,
    ) -> Option<(i32, i32, usize)> {
        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let (icon, at, key) = if unsafe { GetCursorInfo(&mut cursor) }.is_ok()
            && cursor.flags.0 & CURSOR_SHOWING.0 != 0
            && !cursor.hCursor.is_invalid()
        {
            let icon = HICON(cursor.hCursor.0);
            let mut details = ICONINFO::default();
            let hotspot = if unsafe { GetIconInfo(icon, &mut details) }.is_ok() {
                unsafe {
                    if !details.hbmMask.is_invalid() {
                        let _ = DeleteObject(details.hbmMask.into());
                    }
                    if !details.hbmColor.is_invalid() {
                        let _ = DeleteObject(details.hbmColor.into());
                    }
                }
                (details.xHotspot as i32, details.yHotspot as i32)
            } else {
                (0, 0)
            };
            let POINT { x, y } = cursor.ptScreenPos;
            (
                icon,
                (x - origin.0 - hotspot.0, y - origin.1 - hotspot.1),
                (x, y, cursor.hCursor.0 as usize),
            )
        } else if let Some((x, y, true)) = dxgi_pointer {
            let Ok(arrow) = (unsafe { LoadCursorW(None, IDC_ARROW) }) else {
                return None;
            };
            (
                HICON(arrow.0),
                (x, y),
                (x + origin.0, y + origin.1, arrow.0 as usize),
            )
        } else {
            return None;
        };

        let surface = match self.gdi.cast::<IDXGISurface1>() {
            Ok(surface) => surface,
            Err(error) => {
                if !self.cursor_error_logged {
                    warn!("cursor canvas lacks IDXGISurface1: {error}");
                    self.cursor_error_logged = true;
                }
                return Some(key);
            }
        };
        unsafe {
            let dc = match surface.GetDC(false) {
                Ok(dc) => dc,
                Err(error) => {
                    if !self.cursor_error_logged {
                        warn!("cursor GetDC failed: {error}");
                        self.cursor_error_logged = true;
                    }
                    return Some(key);
                }
            };
            let drawn = DrawIconEx(dc, at.0, at.1, icon, 0, 0, 0, None, DI_NORMAL);
            let _ = surface.ReleaseDC(None);
            match drawn {
                Ok(()) => {
                    if !self.cursor_logged {
                        info!("cursor drawn via GDI at ({}, {})", at.0, at.1);
                        self.cursor_logged = true;
                    }
                }
                Err(error) => {
                    if !self.cursor_error_logged {
                        warn!("cursor DrawIconEx failed: {error}");
                        self.cursor_error_logged = true;
                    }
                }
            }
        }
        Some(key)
    }
}

fn current_cursor_key() -> Option<(i32, i32, usize)> {
    let mut cursor = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut cursor) }.is_ok()
        && cursor.flags.0 & CURSOR_SHOWING.0 != 0
        && !cursor.hCursor.is_invalid()
    {
        Some((
            cursor.ptScreenPos.x,
            cursor.ptScreenPos.y,
            cursor.hCursor.0 as usize,
        ))
    } else {
        None
    }
}

/// GPU fullscreen-triangle pass converting Windows DWM's linear scRGB HDR
/// (`R16G16B16A16_FLOAT`, 1.0 = 80 nits) into sRGB `B8G8R8A8_UNORM`.
struct HdrToneMapper {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    rasterizer: ID3D11RasterizerState,
    params: ID3D11Buffer,
}

const HDR_TONEMAP_HLSL: &str = r#"
Texture2D<float4> srcTex : register(t0);
cbuffer Params : register(b0) {
    float sdrScale;
    float3 _pad;
};

struct VSOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float3 linearToSrgb(float3 c) {
    c = saturate(c);
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(max(c, 0.0031308), 1.0 / 2.4) - 0.055;
    return (c <= 0.0031308) ? lo : hi;
}

float4 ps_main(VSOut input) : SV_Target {
    int2 coord = int2(input.pos.xy);
    float3 rgb = srcTex.Load(int3(coord, 0)).rgb;
    rgb = max(rgb, 0.0) / max(sdrScale, 1.0);

    // Preserve SDR colors below the knee; roll off HDR highlights smoothly.
    float y = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    const float knee = 0.85;
    if (y > knee) {
        float over = y - knee;
        float mapped = knee + (1.0 - knee) * over / (over + (1.0 - knee));
        rgb *= mapped / y;
    }
    float peak = max(rgb.r, max(rgb.g, rgb.b));
    if (peak > 1.0) {
        rgb /= peak;
    }
    return float4(linearToSrgb(rgb), 1.0);
}
"#;

impl HdrToneMapper {
    fn new(device: &ID3D11Device, sdr_scale: f32) -> Result<HdrToneMapper> {
        let vs_blob = compile_hlsl(HDR_TONEMAP_HLSL, "vs_main\0", "vs_5_0\0")?;
        let ps_blob = compile_hlsl(HDR_TONEMAP_HLSL, "ps_main\0", "ps_5_0\0")?;

        let mut vertex_shader = None;
        unsafe {
            let bytecode = std::slice::from_raw_parts(
                vs_blob.GetBufferPointer() as *const u8,
                vs_blob.GetBufferSize(),
            );
            device.CreateVertexShader(bytecode, None, Some(&mut vertex_shader))
        }
        .context("creating HDR vertex shader")?;

        let mut pixel_shader = None;
        unsafe {
            let bytecode = std::slice::from_raw_parts(
                ps_blob.GetBufferPointer() as *const u8,
                ps_blob.GetBufferSize(),
            );
            device.CreatePixelShader(bytecode, None, Some(&mut pixel_shader))
        }
        .context("creating HDR pixel shader")?;

        let rast_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: false.into(),
            ..Default::default()
        };
        let mut rasterizer = None;
        unsafe { device.CreateRasterizerState(&rast_desc, Some(&mut rasterizer)) }
            .context("creating HDR rasterizer state")?;

        let cbuf_data: [f32; 4] = [sdr_scale.clamp(1.0, 12.0), 0.0, 0.0, 0.0];
        let cbuf_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<[f32; 4]>() as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: cbuf_data.as_ptr() as *const c_void,
            ..Default::default()
        };
        let mut params = None;
        unsafe { device.CreateBuffer(&cbuf_desc, Some(&init), Some(&mut params)) }
            .context("creating HDR constant buffer")?;

        Ok(HdrToneMapper {
            vertex_shader: vertex_shader.ok_or_else(|| anyhow!("no HDR vertex shader"))?,
            pixel_shader: pixel_shader.ok_or_else(|| anyhow!("no HDR pixel shader"))?,
            rasterizer: rasterizer.ok_or_else(|| anyhow!("no HDR rasterizer state"))?,
            params: params.ok_or_else(|| anyhow!("no HDR constant buffer"))?,
        })
    }

    fn apply(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        target_rtv: &ID3D11RenderTargetView,
        (width, height): (u32, u32),
    ) -> Result<()> {
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(source, None, Some(&mut srv)) }
            .context("creating HDR input SRV")?;
        let srv = srv.ok_or_else(|| anyhow!("no HDR input SRV"))?;

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(srv)]));
            context.RSSetState(&self.rasterizer);
            context.RSSetViewports(Some(&[viewport]));
            context.OMSetRenderTargets(Some(&[Some(target_rtv.clone())]), None);
            context.Draw(3, 0);
            // Unbind so DXGI can reclaim the acquired frame and CopyResource
            // can read `canvas.desktop` cleanly.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

fn compile_hlsl(source: &str, entry_nul: &str, target_nul: &str) -> Result<ID3DBlob> {
    type D3DCompileFn = unsafe extern "system" fn(
        *const c_void,
        usize,
        *const u8,
        *const c_void,
        *const c_void,
        *const u8,
        *const u8,
        u32,
        u32,
        *mut Option<ID3DBlob>,
        *mut Option<ID3DBlob>,
    ) -> HRESULT;

    let module =
        unsafe { LoadLibraryW(w!("d3dcompiler_47.dll")) }.context("loading d3dcompiler_47.dll")?;
    let proc = unsafe { GetProcAddress(module, s!("D3DCompile")) }
        .ok_or_else(|| anyhow!("D3DCompile not found in d3dcompiler_47.dll"))?;
    let compile: D3DCompileFn = unsafe { std::mem::transmute(proc) };

    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        compile(
            source.as_ptr() as *const c_void,
            source.len(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            entry_nul.as_ptr(),
            target_nul.as_ptr(),
            1 << 15, // D3DCOMPILE_OPTIMIZATION_LEVEL3
            0,
            &mut code,
            &mut errors,
        )
    };
    if hr.is_err() {
        let detail = errors
            .map(|blob| unsafe {
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                );
                String::from_utf8_lossy(bytes).trim().to_string()
            })
            .unwrap_or_default();
        bail!("HLSL compile ({entry_nul:?}) failed ({hr}): {detail}");
    }
    code.ok_or_else(|| anyhow!("D3DCompile returned no bytecode"))
}

/// Queries Windows' "SDR content brightness" level for this monitor as a
/// multiplier of 80 nits (scRGB 1.0). Defaults to 3.0 (240 nits) on error.
fn query_sdr_white_scale(
    output: &windows::Win32::Graphics::Dxgi::IDXGIOutput,
    adapter_luid: LUID,
) -> f32 {
    const DEFAULT_SCALE: f32 = 3.0;
    const QDC_ONLY_ACTIVE_PATHS: u32 = 0x0000_0002;
    const DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME: u32 = 1;
    const DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL: u32 = 11;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathSourceInfo {
        adapter_id: LUID,
        id: u32,
        mode_info_idx: u32,
        status_flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathTargetInfo {
        adapter_id: LUID,
        id: u32,
        mode_info_idx: u32,
        output_technology: u32,
        rotation: u32,
        scaling: u32,
        refresh_rate: DXGI_RATIONAL,
        scan_line_ordering: u32,
        target_available: i32,
        status_flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathInfo {
        source: PathSourceInfo,
        target: PathTargetInfo,
        flags: u32,
    }

    #[repr(C, align(8))]
    #[derive(Clone, Copy)]
    struct ModeInfo([u8; 64]);

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct DeviceInfoHeader {
        kind: u32,
        size: u32,
        adapter_id: LUID,
        id: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SourceDeviceName {
        header: DeviceInfoHeader,
        view_gdi_device_name: [u16; 32],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SdrWhiteLevel {
        header: DeviceInfoHeader,
        sdr_white_level: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetDisplayConfigBufferSizes(
            flags: u32,
            num_path_array_elements: *mut u32,
            num_mode_info_array_elements: *mut u32,
        ) -> i32;
        fn QueryDisplayConfig(
            flags: u32,
            num_path_array_elements: *mut u32,
            path_array: *mut PathInfo,
            num_mode_info_array_elements: *mut u32,
            mode_info_array: *mut ModeInfo,
            current_topology_id: *mut c_void,
        ) -> i32;
        fn DisplayConfigGetDeviceInfo(request_packet: *mut DeviceInfoHeader) -> i32;
    }

    let wanted_name = unsafe { output.GetDesc() }.ok().map(|d| d.DeviceName);
    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    } != 0
        || path_count == 0
    {
        return DEFAULT_SCALE;
    }
    let mut paths = vec![PathInfo::default(); path_count as usize];
    let mut modes = vec![ModeInfo([0u8; 64]); mode_count as usize];
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return DEFAULT_SCALE;
    }

    let mut fallback: Option<f32> = None;
    for path in &paths[..path_count as usize] {
        if path.source.adapter_id != adapter_luid {
            continue;
        }
        let mut white = SdrWhiteLevel {
            header: DeviceInfoHeader {
                kind: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<SdrWhiteLevel>() as u32,
                adapter_id: path.target.adapter_id,
                id: path.target.id,
            },
            sdr_white_level: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0
            || white.sdr_white_level == 0
        {
            continue;
        }
        let scale = (white.sdr_white_level as f32 / 1000.0).clamp(1.0, 12.0);
        fallback.get_or_insert(scale);

        if let Some(ref wanted) = wanted_name {
            let mut src_name = SourceDeviceName {
                header: DeviceInfoHeader {
                    kind: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<SourceDeviceName>() as u32,
                    adapter_id: path.source.adapter_id,
                    id: path.source.id,
                },
                view_gdi_device_name: [0u16; 32],
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut src_name.header) } == 0
                && src_name.view_gdi_device_name == *wanted
            {
                return scale;
            }
        }
    }
    fallback.unwrap_or(DEFAULT_SCALE)
}

/// Scales the canvas to the stream size and converts it to NV12, on the GPU.
struct Converter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    surfaces: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        canvas: &ID3D11Texture2D,
        (input_width, input_height): (u32, u32),
        settings: &Settings,
    ) -> Result<Converter> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: settings.fps,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: settings.width,
            OutputHeight: settings.height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("no video processor for this size")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }?;

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                canvas,
                &enumerator,
                &input_desc,
                Some(&mut input),
            )
        }?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut surfaces = Vec::with_capacity(SURFACES);
        for _ in 0..SURFACES {
            let texture = nv12_surface(device, settings.width, settings.height)?;
            let mut view = None;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &output_desc,
                    Some(&mut view),
                )
            }?;
            surfaces.push((texture, view.ok_or_else(|| anyhow!("no output view"))?));
        }

        unsafe {
            // Desktop pixels are full range RGB; the stream is BT.709 limited
            // range, which is what the encoder signals and phones expect.
            let input_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                // YCbCr_Matrix = BT.709 (bit 2), Nominal_Range = 16-235 (bits 4-5).
                _bitfield: (1 << 2) | (1 << 4),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_space);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_space);
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }

        Ok(Converter {
            video_context,
            processor,
            input: input.ok_or_else(|| anyhow!("no input view"))?,
            surfaces,
            next: 0,
        })
    }

    fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (texture, view) = &self.surfaces[self.next];
        self.next = (self.next + 1) % self.surfaces.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let streams = [stream];
        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, view, 0, &streams)
        };
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("converting the desktop to NV12")?;
        Ok(texture.clone())
    }
}

fn nv12_surface(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.is_err() {
        // Older drivers reject the encoder bind flag; encoders accept plain
        // render targets too.
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("creating an NV12 surface")?;
    }
    texture.ok_or_else(|| anyhow!("no NV12 surface"))
}

/// A Media Foundation hardware encoder fed with GPU surfaces.
///
/// Hardware MFTs are asynchronous: they announce with events when they
/// want input and when output is ready. A synchronous one (rare) is driven
/// by trying `ProcessOutput` after each input.
struct Encoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    codec: VideoCodec,
    /// Inputs the encoder asked for and has not received yet.
    wanted: u32,
    /// Frames submitted to the encoder whose output has not been read yet.
    in_flight: u32,
    provides_samples: bool,
    output_size: u32,
    frame_duration: i64,
    /// Parameter sets from the output type, for keyframes that lack them.
    sequence_header: Vec<u8>,
}

impl Encoder {
    fn new(device: &ID3D11Device, luid: LUID, settings: &Settings) -> Result<Encoder> {
        let transform = find_encoder(luid, settings.codec)?;
        let attributes = unsafe { transform.GetAttributes() }?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }?;
        }
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };

        let mut token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }?;
        let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
        unsafe { manager.ResetDevice(device, token) }?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .context("the encoder does not take GPU surfaces")?;

        let codec_api = transform.cast::<ICodecAPI>().ok();
        let mut encoder = Encoder {
            events: asynchronous.then(|| transform.cast().ok()).flatten(),
            transform,
            codec_api,
            _manager: manager,
            codec: settings.codec,
            wanted: 0,
            in_flight: 0,
            provides_samples: false,
            output_size: 0,
            frame_duration: 10_000_000 / i64::from(settings.fps.max(1)),
            sequence_header: Vec::new(),
        };
        encoder.configure(settings)?;
        unsafe {
            encoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            encoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(encoder)
    }

    fn configure(&mut self, settings: &Settings) -> Result<()> {
        // Rate control before the types: some encoders size their buffers
        // from it when the output type is set.
        self.set_codec_value(
            &CODECAPI_AVEncCommonRateControlMode,
            u32_variant(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        self.set_codec_value(
            &CODECAPI_AVEncCommonMeanBitRate,
            u32_variant(settings.bitrate),
        );
        self.set_codec_value(&CODECAPI_AVLowLatencyMode, bool_variant(true));
        self.set_codec_value(&CODECAPI_AVEncMPVDefaultBPictureCount, u32_variant(0));
        // Keyframes come on request; a huge GOP avoids periodic bursts.
        self.set_codec_value(&CODECAPI_AVEncMPVGOPSize, u32_variant(u32::MAX));

        let (subtype, profile) = match settings.codec {
            VideoCodec::H264 => (MFVideoFormat_H264, eAVEncH264VProfile_High.0 as u32),
            VideoCodec::H265 => (MFVideoFormat_HEVC, eAVEncH265VProfile_Main_420_8.0 as u32),
        };
        unsafe {
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            set_video_format(&output, settings)?;
            self.transform
                .SetOutputType(0, &output, 0)
                .context("the encoder rejected the output format")?;

            let input = self.nv12_input_type()?;
            set_video_format(&input, settings)?;
            self.transform
                .SetInputType(0, &input, 0)
                .context("the encoder rejected NV12 input")?;
        }
        self.read_output_details()
    }

    fn nv12_input_type(&self) -> Result<IMFMediaType> {
        for index in 0.. {
            let Ok(candidate) = (unsafe { self.transform.GetInputAvailableType(0, index) }) else {
                break;
            };
            if unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
                return Ok(candidate);
            }
        }
        let input = unsafe { MFCreateMediaType() }?;
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        }
        Ok(input)
    }

    fn read_output_details(&mut self) -> Result<()> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
        self.provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        self.output_size = info.cbSize.max(1 << 20);
        self.sequence_header = unsafe { self.transform.GetOutputCurrentType(0) }
            .ok()
            .and_then(|media_type| blob(&media_type, &MF_MT_MPEG_SEQUENCE_HEADER))
            .unwrap_or_default();
        Ok(())
    }

    fn set_codec_value(&self, api: &GUID, value: VARIANT) {
        if let Some(codec_api) = &self.codec_api {
            if let Err(error) = unsafe { codec_api.SetValue(api, &value) } {
                debug!("encoder ignores setting {api:?}: {error}");
            }
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        self.set_codec_value(&CODECAPI_AVEncCommonMeanBitRate, u32_variant(bitrate));
    }

    fn set_frame_rate(&mut self, fps: u32) {
        // Sample timestamps carry the real pace; this only sizes rate control.
        self.frame_duration = 10_000_000 / i64::from(fps.max(1));
    }

    /// True when the encoder takes another frame now.
    fn ready(&self) -> bool {
        self.events.is_none() || self.wanted > 0
    }

    fn encode(&mut self, surface: &ID3D11Texture2D, time: i64, keyframe: bool) -> Result<()> {
        if keyframe {
            self.set_codec_value(&CODECAPI_AVEncVideoForceKeyFrame, u32_variant(1));
        }
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, surface, 0, false)?;
            if let Ok(planes) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(planes.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.wanted = self.wanted.saturating_sub(1);
        self.in_flight = self.in_flight.saturating_add(1);
        Ok(())
    }

    /// Collects whatever output is ready, without blocking.
    fn poll(&mut self, deliver: &mut dyn FnMut(EncodedFrame)) -> Result<()> {
        let Some(events) = self.events.clone() else {
            while let Some(frame) = self.take_output()? {
                deliver(frame);
            }
            return Ok(());
        };
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let kind = unsafe { event.GetType() }?;
            if kind == METransformNeedInput.0 as u32 {
                self.wanted += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.take_output()? {
                    deliver(frame);
                }
            }
        }
    }

    fn take_output(&mut self) -> Result<Option<EncodedFrame>> {
        let sample = if self.provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample() }?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_size) }?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        let sample = ManuallyDrop::into_inner(output.pSample);
        drop(ManuallyDrop::into_inner(output.pEvents));
        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let media_type = unsafe { self.transform.GetOutputAvailableType(0, 0) }?;
                unsafe { self.transform.SetOutputType(0, &media_type, 0) }?;
                self.read_output_details()?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let micros = (unsafe { sample.GetSampleTime() }.unwrap_or(0) / 10).max(0) as u64;
        let mut data = contiguous_bytes(&sample)?;
        if has_parameter_sets(&data, self.codec) {
            if let Some(extracted) = extract_parameter_sets(&data, self.codec) {
                self.sequence_header = extracted;
            }
        } else if keyframe && !self.sequence_header.is_empty() {
            let mut with_sets = self.sequence_header.clone();
            with_sets.extend_from_slice(&data);
            data = with_sets;
        }
        if keyframe {
            debug!("encoded keyframe: {} bytes", data.len());
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        Ok(Some(EncodedFrame { data, micros }))
    }
}

/// The first hardware encoder for `codec` on the adapter that owns the
/// display; surfaces cannot cross adapters.
fn find_encoder(luid: LUID, codec: VideoCodec) -> Result<IMFTransform> {
    let subtype = match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::H265 => MFVideoFormat_HEVC,
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
    }?;
    if list.is_null() || count == 0 {
        bail!("no hardware {codec} encoder on this machine");
    }
    let activates = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    let wanted = luid_bytes(luid);
    let chosen = activates
        .iter()
        .flatten()
        .find(|activate| blob(activate, &MFT_ENUM_ADAPTER_LUID).as_deref() == Some(&wanted[..]))
        .or_else(|| activates.iter().flatten().next())
        .cloned();
    for activate in activates.iter_mut() {
        drop(activate.take());
    }
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    let activate = chosen.ok_or_else(|| anyhow!("no hardware {codec} encoder"))?;
    if let Some(name) = friendly_name(&activate) {
        info!("encoder: {name}");
    }
    Ok(unsafe { activate.ActivateObject::<IMFTransform>() }?)
}

fn luid_bytes(luid: LUID) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    bytes
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut length = 0;
    unsafe { activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }
        .ok()
        .and_then(|len| {
            let mut buffer = vec![0u16; len as usize + 1];
            unsafe {
                activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buffer, Some(&mut length))
            }
            .ok()?;
            Some(String::from_utf16_lossy(&buffer[..length as usize]))
        })
}

fn blob(attributes: &IMFAttributes, key: &GUID) -> Option<Vec<u8>> {
    let size = unsafe { attributes.GetBlobSize(key) }.ok()?;
    let mut buffer = vec![0u8; size as usize];
    unsafe { attributes.GetBlob(key, &mut buffer, None) }.ok()?;
    Some(buffer)
}

unsafe fn set_video_format(media_type: &IMFMediaType, settings: &Settings) -> Result<()> {
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(settings.width) << 32) | u64::from(settings.height),
        )?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(settings.fps) << 32) | 1)?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

fn contiguous_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock()?;
        Ok(bytes)
    }
}

/// True when an Annex-B access unit carries its parameter sets.
fn has_parameter_sets(data: &[u8], codec: VideoCodec) -> bool {
    extract_parameter_sets(data, codec).is_some()
}

/// Extracts all parameter-set NAL units (SPS/PPS or VPS/SPS/PPS) in Annex-B
/// form so subsequent forced keyframes can carry them if the encoder omits them.
fn extract_parameter_sets(data: &[u8], codec: VideoCodec) -> Option<Vec<u8>> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            let prefix_start = if index > 0 && data[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            starts.push((prefix_start, index + 3));
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut out = Vec::new();
    for (i, &(prefix_start, nal_start)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(data.len(), |&(next, _)| next);
        if nal_start >= end {
            continue;
        }
        let header = data[nal_start];
        let parameter_set = match codec {
            VideoCodec::H264 => matches!(header & 0x1f, 7 | 8),
            VideoCodec::H265 => matches!((header >> 1) & 0x3f, 32..=34),
        };
        if parameter_set {
            out.extend_from_slice(&data[prefix_start..end]);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn u32_variant(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

fn bool_variant(value: bool) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value {
            VARIANT_TRUE
        } else {
            Default::default()
        };
    }
    variant
}

/// Microseconds on the performance counter, the clock audio is stamped with.
pub struct Clock {
    frequency: i64,
}

impl Clock {
    pub fn new() -> Clock {
        let mut frequency = 0;
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        Clock {
            frequency: frequency.max(1),
        }
    }

    pub fn micros(&self) -> u64 {
        let mut now = 0;
        let _ = unsafe { QueryPerformanceCounter(&mut now) };
        (i128::from(now) * 1_000_000 / i128::from(self.frequency)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_parameter_sets_in_annex_b() {
        let h264_idr = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4];
        let h264_p = [0, 0, 0, 1, 0x41, 9];
        assert!(has_parameter_sets(&h264_idr, VideoCodec::H264));
        assert!(!has_parameter_sets(&h264_p, VideoCodec::H264));
        let hevc_vps = [0, 0, 1, 32 << 1, 1, 0];
        assert!(has_parameter_sets(&hevc_vps, VideoCodec::H265));
    }
}
