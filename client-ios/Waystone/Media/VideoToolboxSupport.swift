import VideoToolbox

/// Which video codec the local hardware can decode.
///
/// The host prefers HEVC because it is roughly half the bitrate for the same
/// quality, but the codec has to be agreed through SDP, so the controller only
/// offers what it can actually decode. The iOS simulator, for instance, has no
/// HEVC decoder, and asking for it produces a black screen instead of an error.
enum VideoToolboxSupport {
    /// `h265` when this device decodes HEVC in hardware, `h264` otherwise.
    static func preferredCodec() -> String {
        VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC) ? "h265" : "h264"
    }
}
