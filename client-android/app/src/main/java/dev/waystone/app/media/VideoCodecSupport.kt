package dev.waystone.app.media

import android.media.MediaCodecList
import android.media.MediaFormat
import dev.waystone.app.protocol.Wire

object VideoCodecSupport {
    /**
     * Prefers hardware HEVC when available on the device, falling back to H.264.
     */
    fun bestDecodable(): Wire.VideoCodec {
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
        val hasHwHevc = list.codecInfos.any { info ->
            !info.isEncoder &&
                info.isHardwareAccelerated &&
                info.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_VIDEO_HEVC, ignoreCase = true) }
        }
        return if (hasHwHevc) Wire.VideoCodec.H265 else Wire.VideoCodec.H264
    }
}
