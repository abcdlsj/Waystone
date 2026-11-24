import Foundation

/// Swift face of the Rust transport (`crates/waystone-rtc`, C ABI in
/// `waystone_rtc.h`).
///
/// ICE, DTLS, RTP and the data channels all live in Rust so the phone and the
/// host share one implementation. Callbacks arrive on a Rust thread: media is
/// handed straight to `MediaSink` there (hopping to the main thread per frame
/// would add latency for nothing), control events go to `onControl`, which the
/// caller is expected to hop to the main actor.
final class Transport {
    enum ControlEvent {
        case localOffer(String)
        case localCandidate(String)
        case connected
        case disconnected
        case path(relayed: Bool, rttMs: UInt32?)
        case closed(String)
    }

    struct Configuration {
        var audio: Bool
        var codec: Wire.VideoCodec
        /// STUN server, `host:port`.
        var stun: String?
        var turn: Wire.TurnCredentials?
    }

    private var handle: OpaquePointer?
    private let callbacks: Callbacks

    /// Starts the engine; the controller side offers immediately.
    init(
        _ configuration: Configuration, media: MediaSink,
        onControl: @escaping (ControlEvent) -> Void
    ) throws {
        callbacks = Callbacks(media: media, onControl: onControl)

        var config: [String: Any] = [
            "role": "controller",
            "audio": configuration.audio,
            "video_codec": configuration.codec.rawValue,
            "bind": "0.0.0.0:0",
        ]
        if let stun = configuration.stun { config["stun"] = stun }
        if let turn = configuration.turn {
            config["turn"] = [
                "server": turn.server, "username": turn.username, "password": turn.password,
            ]
        }
        let json = String(decoding: try JSONSerialization.data(withJSONObject: config), as: UTF8.self)

        var error = [CChar](repeating: 0, count: 512)
        let context = Unmanaged.passUnretained(callbacks).toOpaque()
        handle = json.withCString { ws_engine_start($0, transportCallback, context, &error, error.count) }
        guard handle != nil else {
            throw TransportError.start(String(cString: error))
        }
    }

    deinit { stop() }

    func acceptAnswer(_ sdp: String) {
        guard let handle else { return }
        sdp.withCString { ws_engine_accept_answer(handle, $0) }
    }

    func addCandidate(_ candidate: String) {
        guard let handle else { return }
        candidate.withCString { ws_engine_add_candidate(handle, $0) }
    }

    func send(_ event: InputEvent) {
        guard let handle else { return }
        let frame = event.encoded()
        frame.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            ws_engine_send_input(handle, event.isReliable, base, raw.count)
        }
    }

    /// Stops the engine. No callback runs once this returns, which is what
    /// makes the unretained callback context safe.
    func stop() {
        guard let handle else { return }
        self.handle = nil
        ws_engine_stop(handle)
    }
}

enum TransportError: LocalizedError {
    case start(String)

    var errorDescription: String? {
        switch self {
        case .start(let reason): return "无法启动传输：\(reason)"
        }
    }
}

/// Receives media on the transport thread.
protocol MediaSink: AnyObject {
    func video(_ frame: UnsafeBufferPointer<UInt8>, keyframe: Bool, rtpTime: UInt64)
    func audio(_ packet: UnsafeBufferPointer<UInt8>)
}

/// The callback context; split from `Transport` so stopping the engine from
/// its own deinit never races a callback into a half-destroyed object.
private final class Callbacks {
    let media: MediaSink
    let onControl: (Transport.ControlEvent) -> Void

    init(media: MediaSink, onControl: @escaping (Transport.ControlEvent) -> Void) {
        self.media = media
        self.onControl = onControl
    }

    func dispatch(kind: Int32, bytes: UnsafeBufferPointer<UInt8>) {
        switch UInt32(bitPattern: kind) {
        case WS_EVENT_VIDEO.rawValue:
            guard let (header, payload) = MediaHeader.split(bytes) else { return }
            media.video(payload, keyframe: header.keyframe, rtpTime: header.rtpTime)
        case WS_EVENT_AUDIO.rawValue:
            guard let (_, payload) = MediaHeader.split(bytes) else { return }
            media.audio(payload)
        case WS_EVENT_CONTROL.rawValue:
            if let event = Self.control(from: Data(bytes)) { onControl(event) }
        default:
            break
        }
    }

    private static func control(from data: Data) -> Transport.ControlEvent? {
        guard let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
            let kind = json["kind"] as? String
        else { return nil }
        switch kind {
        case "local_offer": return (json["sdp"] as? String).map { .localOffer($0) }
        case "local_candidate": return (json["candidate"] as? String).map { .localCandidate($0) }
        case "connected": return .connected
        case "disconnected": return .disconnected
        case "path_update":
            return .path(
                relayed: json["relayed"] as? Bool ?? false,
                rttMs: (json["rttMs"] as? NSNumber)?.uint32Value)
        case "closed": return .closed(json["reason"] as? String ?? "")
        default: return nil
        }
    }
}

private func transportCallback(
    kind: Int32, data: UnsafePointer<UInt8>?, length: Int, context: UnsafeMutableRawPointer?
) {
    guard let context else { return }
    let callbacks = Unmanaged<Callbacks>.fromOpaque(context).takeUnretainedValue()
    callbacks.dispatch(kind: kind, bytes: UnsafeBufferPointer(start: data, count: data == nil ? 0 : length))
}

/// The 16 byte header in front of every media event, `ffi.rs::MediaHeader`.
struct MediaHeader {
    var flags: UInt8
    var rtpTime: UInt64

    var keyframe: Bool { flags & 1 != 0 }

    static let length = 16

    static func split(_ bytes: UnsafeBufferPointer<UInt8>) -> (MediaHeader, UnsafeBufferPointer<UInt8>)? {
        guard bytes.count >= length, let base = bytes.baseAddress else { return nil }
        var time: UInt64 = 0
        for index in 2..<10 { time = time << 8 | UInt64(bytes[index]) }
        let payload = UnsafeBufferPointer(start: base + length, count: bytes.count - length)
        return (MediaHeader(flags: bytes[1], rtpTime: time), payload)
    }
}
