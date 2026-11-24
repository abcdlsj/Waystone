import VideoToolbox

extension Wire.VideoCodec {
    /// HEVC when this device decodes it in hardware, H.264 otherwise.
    ///
    /// HEVC is about half the bitrate for the same picture, but the iOS
    /// simulator has no HEVC decoder at all, and asking for it there gives a
    /// black screen instead of an error.
    static var bestDecodable: Wire.VideoCodec {
        VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC) ? .h265 : .h264
    }
}
