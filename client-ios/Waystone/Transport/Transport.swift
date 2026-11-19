import Foundation

/// Thin Swift wrapper around the Rust transport (`crates/waystone-rtc`).
///
/// All the networking, ICE, DTLS and RTP work happens in Rust so the phone and the
/// host cannot drift apart. This type only marshals frames across the C boundary.
final class Transport {
    /// Events the transport surfaces.
    enum Event {
        case localOffer(String)
        case localAnswer(String)
        case localCandidate(String)
        case connected
        case disconnected
        case mediaReady
        case pathUpdate(rttMs: UInt32?, relayed: Bool, estimatedBitrate: UInt32?)
        case keyframeRequest
        case closed(String)
        case video(data: [UInt8], keyframe: Bool, rtpTime: UInt64)
        case audio(data: [UInt8], rtpTime: UInt64)
        case input(data: [UInt8], reliable: Bool)
    }

    private var handle: OpaquePointer?
    private let handler: (Event) -> Void
    private var retainedSelf: UnsafeMutableRawPointer?

    /// Starts the transport. Returns nil when the engine cannot bind.
    init?(
        role: String,
        viewport: Wire.Viewport?,
        audio: Bool,
        stun: String?,
        turn: (server: String, username: String, password: String)?,
        handler: @escaping (Event) -> Void
    ) {
        self.handler = handler

        var config: [String: Any] = ["role": role, "audio": audio]
        if let viewport {
            config["viewport"] = [
                "width": viewport.width,
                "height": viewport.height,
                "dpi": viewport.dpi,
                "fps": viewport.fps,
            ]
        }
        if let stun { config["stun"] = stun }
        if let turn {
            config["turn"] = [
                "server": turn.server,
                "username": turn.username,
                "password": turn.password,
                "relay_bitrate_ceiling": 4_000_000,
            ]
        }
        config["bind"] = "0.0.0.0:0"

        guard let json = try? JSONSerialization.data(withJSONObject: config),
            let jsonText = String(data: json, encoding: .utf8)
        else { return nil }

        // The context is `self`: the Rust side calls back into the trampoline with
        // it, and the unmanaged reference keeps the object alive for the lifetime
        // of the handle.
        let context = Unmanaged.passUnretained(self).toOpaque()
        self.retainedSelf = context

        var errorBuffer = [CChar](repeating: 0, count: 512)
        let handle = jsonText.withCString { configPointer in
            ws_engine_start(configPointer, transportCallback, context, &errorBuffer, errorBuffer.count)
        }
        guard let handle else {
            NSLog("waystone: transport failed to start: \(String(cString: errorBuffer))")
            return nil
        }
        self.handle = handle
    }

    deinit {
        if let handle {
            ws_engine_stop(handle)
        }
    }

    var localPort: String {
        guard let handle else { return "" }
        var buffer = [CChar](repeating: 0, count: 16)
        let written = ws_engine_local_port(handle, &buffer, buffer.count)
        return written > 0 ? String(cString: buffer) : ""
    }

    func offer() {
        guard let handle else { return }
        ws_engine_offer(handle)
    }

    func acceptOffer(_ sdp: String) {
        guard let handle else { return }
        sdp.withCString { ws_engine_accept_offer(handle, $0) }
    }

    func acceptAnswer(_ sdp: String) {
        guard let handle else { return }
        sdp.withCString { ws_engine_accept_answer(handle, $0) }
    }

    func addCandidate(_ candidate: String) {
        guard let handle else { return }
        candidate.withCString { ws_engine_add_candidate(handle, $0) }
    }

    func sendInput(_ frame: Data, reliable: Bool) {
        guard let handle, !frame.isEmpty else { return }
        frame.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            ws_engine_send_input(handle, reliable, base, frame.count)
        }
    }

    func setVideoBitrate(_ bitsPerSecond: UInt32) {
        guard let handle else { return }
        ws_engine_set_video_bitrate(handle, bitsPerSecond)
    }

    func stop() {
        guard let handle else { return }
        ws_engine_stop(handle)
        self.handle = nil
    }

    /// Dispatches a callback from Rust.
    fileprivate func dispatch(kind: Int32, bytes: [UInt8]) {
        switch kind {
        case Int32(WS_EVENT_VIDEO.rawValue), Int32(WS_EVENT_AUDIO.rawValue):
            guard let (header, payload) = MediaHeader.parse(bytes) else { return }
            if kind == Int32(WS_EVENT_VIDEO.rawValue) {
                handler(.video(data: payload, keyframe: header.keyframe, rtpTime: header.rtpTime))
            } else {
                handler(.audio(data: payload, rtpTime: header.rtpTime))
            }
        case Int32(WS_EVENT_INPUT.rawValue):
            guard let (header, payload) = MediaHeader.parse(bytes) else { return }
            handler(.input(data: payload, reliable: header.reliable))
        case Int32(WS_EVENT_CONTROL.rawValue):
            guard let json = try? JSONSerialization.jsonObject(with: Data(bytes)) as? [String: Any],
                let kind = json["kind"] as? String
            else { return }
            dispatchControl(kind: kind, json: json)
        default:
            break
        }
    }

    private func dispatchControl(kind: String, json: [String: Any]) {
        switch kind {
        case "local_offer":
            handler(.localOffer(json["sdp"] as? String ?? ""))
        case "local_answer":
            handler(.localAnswer(json["sdp"] as? String ?? ""))
        case "local_candidate":
            handler(.localCandidate(json["candidate"] as? String ?? ""))
        case "connected":
            handler(.connected)
        case "disconnected":
            handler(.disconnected)
        case "media_ready":
            handler(.mediaReady)
        case "path_update":
            handler(
                .pathUpdate(
                    rttMs: (json["rttMs"] as? NSNumber)?.uint32Value,
                    relayed: json["relayed"] as? Bool ?? false,
                    estimatedBitrate: (json["estimatedBitrate"] as? NSNumber)?.uint32Value))
        case "keyframe_request":
            handler(.keyframeRequest)
        case "closed":
            handler(.closed(json["reason"] as? String ?? ""))
        default:
            break
        }
    }
}

/// C callback trampoline; the context is the `Transport` instance.
private func transportCallback(
    kind: Int32,
    data: UnsafePointer<UInt8>?,
    length: Int,
    context: UnsafeMutableRawPointer?
) {
    guard let context else { return }
    let transport = Unmanaged<Transport>.fromOpaque(context).takeUnretainedValue()
    let bytes: [UInt8]
    if let data, length > 0 {
        bytes = Array(UnsafeBufferPointer(start: data, count: length))
    } else {
        bytes = []
    }
    transport.dispatch(kind: kind, bytes: bytes)
}

/// Decoded media header shared with `crates/waystone-rtc/src/ffi.rs`.
struct MediaHeader {
    var kind: UInt8
    var flags: UInt8
    var rtpTime: UInt64

    var keyframe: Bool { flags & 1 != 0 }
    var reliable: Bool { flags & 2 != 0 }

    static let length = 16

    static func parse(_ bytes: [UInt8]) -> (MediaHeader, [UInt8])? {
        guard bytes.count >= length else { return nil }
        var time: UInt64 = 0
        for index in 2..<10 {
            time = time << 8 | UInt64(bytes[index])
        }
        let header = MediaHeader(kind: bytes[0], flags: bytes[1], rtpTime: time)
        return (header, Array(bytes[length...]))
    }
}
