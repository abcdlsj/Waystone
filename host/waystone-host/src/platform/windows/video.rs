//! The video half of a capture session, all on one thread and one GPU:
//! desktop duplication → a BGRA canvas with the cursor drawn in → the video
//! processor (scale and BGRA to NV12) → a Media Foundation hardware encoder.
//!
//! No frame ever leaves video memory (design doc 4.3). The encoder runs
//! without B-frames, with an unlimited GOP and keyframes only on request,
//! and when the desktop is still the last picture is re-encoded at 5 fps so
//! a viewer that just joined, or lost a packet, gets a picture.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, info};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{HMODULE, LUID, POINT, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGISurface1, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HICON, ICONINFO,
};

use waystone_proto::VideoCodec;

use super::system::{find_output, Output};

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
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<Duplication>,
    canvas: Option<Canvas>,
    converter: Option<Converter>,
    encoder: Encoder,
    settings: Settings,
    clock: Clock,
    /// A picture is on the canvas (the first frame arrived).
    has_picture: bool,
    /// The canvas changed since it was last encoded.
    dirty: bool,
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
            device,
            context,
            duplication: None,
            canvas: None,
            converter: None,
            encoder,
            settings,
            clock: Clock::new(),
            has_picture: false,
            dirty: false,
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

    /// One turn of the loop: collect encoder output, take a desktop frame
    /// if there is one, and encode when a frame is due.
    pub fn step(&mut self, deliver: &mut dyn FnMut(EncodedFrame)) -> Result<()> {
        self.encoder.poll(deliver)?;

        match self.acquire() {
            Ok(()) => {}
            Err(error) => {
                // Lock screen, UAC prompt, mode change or a fullscreen game
                // switching modes. Keep the last picture and retry.
                debug!("desktop duplication interrupted: {error:#}");
                self.duplication = None;
                std::thread::sleep(Duration::from_millis(100));
                if let Err(error) = self.reconnect() {
                    debug!("desktop duplication unavailable: {error:#}");
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
        }

        let now = Instant::now();
        let interval = Duration::from_secs(1) / self.settings.fps.max(1);
        let since_encode = self.last_encode.map(|at| now - at);
        // A little early is better than a whole interval late.
        let due = since_encode.is_none_or(|elapsed| elapsed + interval / 8 >= interval);
        let idle = now - self.last_change >= IDLE_AFTER;
        let keepalive = idle && since_encode.is_none_or(|elapsed| elapsed >= KEEPALIVE);
        if self.has_picture && self.encoder.ready() && ((self.dirty && due) || keepalive) {
            self.encode(now)?;
        }
        Ok(())
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
    fn acquire(&mut self) -> Result<()> {
        let Some(duplication) = self.duplication.as_ref().map(|d| d.handle.clone()) else {
            bail!("not duplicating");
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { duplication.AcquireNextFrame(4, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
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
        // Neither the image nor the pointer changed.
        if info.LastPresentTime == 0 && info.LastMouseUpdateTime == 0 && self.has_picture {
            return Ok(());
        }
        let texture: ID3D11Texture2D = resource
            .ok_or_else(|| anyhow!("frame without a surface"))?
            .cast()?;
        let canvas = self.canvas.as_ref().expect("duplication implies a canvas");
        unsafe { self.context.CopyResource(&canvas.texture, &texture) };
        canvas.draw_cursor(self.duplication.as_ref().unwrap().origin);
        self.has_picture = true;
        self.dirty = true;
        self.last_change = Instant::now();
        Ok(())
    }

    /// (Re)creates the duplication, and the canvas and converter when the
    /// desktop size changed.
    fn reconnect(&mut self) -> Result<()> {
        let output: IDXGIOutput1 = self.output.output.cast()?;
        let handle = unsafe { output.DuplicateOutput(&self.device) }?;
        let desc = unsafe { handle.GetDesc() };
        let size = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        let origin = unsafe { self.output.output.GetDesc() }?.DesktopCoordinates;
        if self.canvas.as_ref().map(|canvas| canvas.size) != Some(size) {
            info!("desktop is {}x{}", size.0, size.1);
            let canvas = Canvas::new(&self.device, size)?;
            self.converter = Some(Converter::new(
                &self.device,
                &self.context,
                &canvas.texture,
                size,
                &self.settings,
            )?);
            self.canvas = Some(canvas);
            self.has_picture = false;
        }
        self.duplication = Some(Duplication {
            handle,
            origin: (origin.left, origin.top),
        });
        self.keyframe_wanted = true;
        Ok(())
    }
}

struct Duplication {
    handle: IDXGIOutputDuplication,
    /// Desktop coordinates of the output's top left corner.
    origin: (i32, i32),
}

/// The latest desktop image. GDI compatible, because desktop duplication
/// leaves the pointer out and GDI is the one API that draws every kind of
/// cursor (colour, monochrome, animated) correctly.
struct Canvas {
    texture: ID3D11Texture2D,
    size: (u32, u32),
}

impl Canvas {
    fn new(device: &ID3D11Device, (width, height): (u32, u32)) -> Result<Canvas> {
        let desc = D3D11_TEXTURE2D_DESC {
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
            MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("creating the desktop canvas")?;
        Ok(Canvas {
            texture: texture.ok_or_else(|| anyhow!("no canvas texture"))?,
            size: (width, height),
        })
    }

    fn draw_cursor(&self, origin: (i32, i32)) {
        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut cursor) }.is_err() || cursor.flags.0 & CURSOR_SHOWING.0 == 0
        {
            return;
        }
        let icon = HICON(cursor.hCursor.0);
        let mut details = ICONINFO::default();
        if unsafe { GetIconInfo(icon, &mut details) }.is_err() {
            return;
        }
        unsafe {
            if !details.hbmMask.is_invalid() {
                let _ = DeleteObject(details.hbmMask.into());
            }
            if !details.hbmColor.is_invalid() {
                let _ = DeleteObject(details.hbmColor.into());
            }
        }
        let POINT { x, y } = cursor.ptScreenPos;
        let at = (
            x - origin.0 - details.xHotspot as i32,
            y - origin.1 - details.yHotspot as i32,
        );
        let Ok(surface) = self.texture.cast::<IDXGISurface1>() else {
            return;
        };
        unsafe {
            let Ok(dc) = surface.GetDC(false) else {
                return;
            };
            let _ = DrawIconEx(dc, at.0, at.1, icon, 0, 0, 0, None, DI_NORMAL);
            let _ = surface.ReleaseDC(None);
        }
    }
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
        if keyframe && !has_parameter_sets(&data, self.codec) && !self.sequence_header.is_empty() {
            let mut with_sets = self.sequence_header.clone();
            with_sets.extend_from_slice(&data);
            data = with_sets;
        }
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
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            let header = data[index + 3];
            let parameter_set = match codec {
                VideoCodec::H264 => matches!(header & 0x1f, 7 | 8),
                VideoCodec::H265 => matches!((header >> 1) & 0x3f, 32..=34),
            };
            if parameter_set {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
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
