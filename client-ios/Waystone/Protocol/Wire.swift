import Foundation

/// Swift mirror of `crates/waystone-proto/src/signal.rs`.
///
/// Every message is one JSON object tagged by `type`, exactly as serde writes
/// it. Every multi-word field spells its snake_case wire name in `CodingKeys`,
/// so a mismatch with the Rust side fails in one decoder instead of turning
/// into silently missing data.
enum Wire {
    static let protocolVersion: UInt32 = 1

    // MARK: - Shared values

    enum Role: String, Codable {
        case host
        case controller
    }

    enum Platform: String, Codable {
        case windows, macos, linux, ios, ipados, android, other

        init(from decoder: Decoder) throws {
            let raw = try decoder.singleValueContainer().decode(String.self)
            self = Platform(rawValue: raw) ?? .other
        }
    }

    enum VideoCodec: String, Codable {
        case h264
        case h265
    }

    struct Viewport: Codable, Equatable {
        var width: UInt32
        var height: UInt32
        var dpi: UInt16
        var fps: UInt32
    }

    struct DeviceInfo: Codable, Equatable {
        var id: String
        var name: String
        var platform: Platform
        var role: Role
        var publicKey: String

        private enum CodingKeys: String, CodingKey {
            case id, name, platform, role
            case publicKey = "public_key"
        }
    }

    struct Identity: Codable {
        var id: String
        var publicKey: String

        private enum CodingKeys: String, CodingKey {
            case id
            case publicKey = "public_key"
        }
    }

    struct TurnCredentials: Codable, Equatable {
        var server: String
        var username: String
        var password: String
        var relayBitrateCeiling: UInt32?

        private enum CodingKeys: String, CodingKey {
            case server, username, password
            case relayBitrateCeiling = "relay_bitrate_ceiling"
        }
    }

    struct PeerState: Codable, Equatable {
        var device: DeviceInfo
        var online: Bool
        var rttMs: UInt32?
        var busy: Bool

        private enum CodingKeys: String, CodingKey {
            case device, online, busy
            case rttMs = "rtt_ms"
        }

        init(from decoder: Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            device = try container.decode(DeviceInfo.self, forKey: .device)
            online = try container.decode(Bool.self, forKey: .online)
            rttMs = try container.decodeIfPresent(UInt32.self, forKey: .rttMs)
            busy = try container.decodeIfPresent(Bool.self, forKey: .busy) ?? false
        }

        init(device: DeviceInfo, online: Bool, rttMs: UInt32? = nil, busy: Bool = false) {
            self.device = device
            self.online = online
            self.rttMs = rttMs
            self.busy = busy
        }
    }

    enum ByeReason: String, Codable {
        case userHangup = "user_hangup"
        case lockAndExit = "lock_and_exit"
        case networkTimeout = "network_timeout"
        case peerOffline = "peer_offline"
        case hostError = "host_error"
        /// A reason added by a newer server.
        case unknown

        init(from decoder: Decoder) throws {
            let raw = try decoder.singleValueContainer().decode(String.self)
            self = ByeReason(rawValue: raw) ?? .unknown
        }
    }

    /// Must match `waystone_proto::identity::Challenge::signing_payload`.
    static func signingPayload(serverId: String, nonce: String) -> Data {
        Data("waystone-auth-v1|\(serverId)|\(nonce)".utf8)
    }

    // MARK: - Session payloads

    enum RtcSignal: Codable, Equatable {
        case offer(sdp: String)
        case answer(sdp: String)
        case ice(candidate: String)
        case iceComplete

        private enum Keys: String, CodingKey {
            case kind, sdp, candidate
            case sdpMid = "sdp_mid"
            case sdpMlineIndex = "sdp_mline_index"
        }

        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            switch try c.decode(String.self, forKey: .kind) {
            case "offer": self = .offer(sdp: try c.decode(String.self, forKey: .sdp))
            case "answer": self = .answer(sdp: try c.decode(String.self, forKey: .sdp))
            case "ice": self = .ice(candidate: try c.decode(String.self, forKey: .candidate))
            case "ice_complete": self = .iceComplete
            case let other:
                throw DecodingError.dataCorruptedError(
                    forKey: .kind, in: c, debugDescription: "unknown rtc signal \(other)")
            }
        }

        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: Keys.self)
            switch self {
            case .offer(let sdp):
                try c.encode("offer", forKey: .kind)
                try c.encode(sdp, forKey: .sdp)
            case .answer(let sdp):
                try c.encode("answer", forKey: .kind)
                try c.encode(sdp, forKey: .sdp)
            case .ice(let candidate):
                try c.encode("ice", forKey: .kind)
                try c.encode(candidate, forKey: .candidate)
                try c.encodeNil(forKey: .sdpMid)
                try c.encodeNil(forKey: .sdpMlineIndex)
            case .iceComplete:
                try c.encode("ice_complete", forKey: .kind)
            }
        }
    }

    enum SessionControl: Codable, Equatable {
        case setFrameRate(fps: UInt32)
        case requestKeyframe
        case setVideoEnabled(Bool)
        case setAudioEnabled(Bool)
        case selectDisplay(index: UInt32)

        private enum Keys: String, CodingKey { case kind, fps, enabled, index }

        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            switch try c.decode(String.self, forKey: .kind) {
            case "set_frame_rate": self = .setFrameRate(fps: try c.decode(UInt32.self, forKey: .fps))
            case "request_keyframe": self = .requestKeyframe
            case "set_video_enabled": self = .setVideoEnabled(try c.decode(Bool.self, forKey: .enabled))
            case "set_audio_enabled": self = .setAudioEnabled(try c.decode(Bool.self, forKey: .enabled))
            case "select_display": self = .selectDisplay(index: try c.decode(UInt32.self, forKey: .index))
            case let other:
                throw DecodingError.dataCorruptedError(
                    forKey: .kind, in: c, debugDescription: "unknown control \(other)")
            }
        }

        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: Keys.self)
            switch self {
            case .setFrameRate(let fps):
                try c.encode("set_frame_rate", forKey: .kind)
                try c.encode(fps, forKey: .fps)
            case .requestKeyframe:
                try c.encode("request_keyframe", forKey: .kind)
            case .setVideoEnabled(let enabled):
                try c.encode("set_video_enabled", forKey: .kind)
                try c.encode(enabled, forKey: .enabled)
            case .setAudioEnabled(let enabled):
                try c.encode("set_audio_enabled", forKey: .kind)
                try c.encode(enabled, forKey: .enabled)
            case .selectDisplay(let index):
                try c.encode("select_display", forKey: .kind)
                try c.encode(index, forKey: .index)
            }
        }
    }

    // MARK: - Client to server

    enum ClientMessage: Encodable {
        case pairRequest(token: String, device: DeviceInfo)
        case auth(identity: Identity, signature: String, name: String)
        case sessionStart(target: String, viewport: Viewport, audio: Bool, videoCodec: VideoCodec)
        case rtcSignal(sessionId: String, signal: RtcSignal)
        case sessionControl(sessionId: String, control: SessionControl)
        case sessionBye(sessionId: String, reason: ByeReason)

        private enum Keys: String, CodingKey {
            case type, token, device, identity, signature, name, target, viewport, audio
            case protocolVersion = "protocol_version"
            case videoCodec = "video_codec"
            case sessionId = "session_id"
            case signal, control, reason
        }

        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: Keys.self)
            switch self {
            case .pairRequest(let token, let device):
                try c.encode("pair_request", forKey: .type)
                try c.encode(token, forKey: .token)
                try c.encode(device, forKey: .device)
            case .auth(let identity, let signature, let name):
                try c.encode("auth", forKey: .type)
                try c.encode(identity, forKey: .identity)
                try c.encode(signature, forKey: .signature)
                try c.encode(Wire.protocolVersion, forKey: .protocolVersion)
                try c.encode(name, forKey: .name)
            case .sessionStart(let target, let viewport, let audio, let codec):
                try c.encode("session_start", forKey: .type)
                try c.encode(target, forKey: .target)
                try c.encode(viewport, forKey: .viewport)
                try c.encode(audio, forKey: .audio)
                try c.encode(codec, forKey: .videoCodec)
            case .rtcSignal(let sessionId, let signal):
                try c.encode("rtc_signal", forKey: .type)
                try c.encode(sessionId, forKey: .sessionId)
                try c.encode(signal, forKey: .signal)
            case .sessionControl(let sessionId, let control):
                try c.encode("session_control", forKey: .type)
                try c.encode(sessionId, forKey: .sessionId)
                try c.encode(control, forKey: .control)
            case .sessionBye(let sessionId, let reason):
                try c.encode("session_bye", forKey: .type)
                try c.encode(sessionId, forKey: .sessionId)
                try c.encode(reason, forKey: .reason)
            }
        }

        func json() -> String {
            // Encoding a value made of strings and integers cannot fail.
            let data = try! Wire.encoder.encode(self)
            return String(decoding: data, as: UTF8.self)
        }
    }

    // MARK: - Server to client

    enum ServerMessage: Decodable {
        case challenge(nonce: String, serverId: String)
        case pairAccepted(serverId: String)
        case pairRejected(reason: String)
        case authOk(peers: [PeerState], turn: TurnCredentials?)
        case authRejected(reason: String)
        case peerUpdate(peers: [PeerState])
        case sessionAccepted(sessionId: String, peer: DeviceInfo, videoCodec: VideoCodec)
        case sessionRejected(reason: String)
        case rtcSignal(sessionId: String, signal: RtcSignal)
        case sessionControl(sessionId: String, control: SessionControl)
        case sessionEnded(sessionId: String, reason: ByeReason, detail: String?)
        case pong
        case error(message: String)

        private enum Keys: String, CodingKey {
            case type, nonce, reason, peers, turn, peer, signal, control, detail, message
            case serverId = "server_id"
            case sessionId = "session_id"
            case videoCodec = "video_codec"
        }

        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            switch try c.decode(String.self, forKey: .type) {
            case "challenge":
                self = .challenge(
                    nonce: try c.decode(String.self, forKey: .nonce),
                    serverId: try c.decode(String.self, forKey: .serverId))
            case "pair_accepted":
                self = .pairAccepted(serverId: try c.decode(String.self, forKey: .serverId))
            case "pair_rejected":
                self = .pairRejected(reason: try c.decode(String.self, forKey: .reason))
            case "auth_ok":
                self = .authOk(
                    peers: try c.decode([PeerState].self, forKey: .peers),
                    turn: try c.decodeIfPresent(TurnCredentials.self, forKey: .turn))
            case "auth_rejected":
                self = .authRejected(reason: try c.decode(String.self, forKey: .reason))
            case "peer_update":
                self = .peerUpdate(peers: try c.decode([PeerState].self, forKey: .peers))
            case "session_accepted":
                self = .sessionAccepted(
                    sessionId: try c.decode(String.self, forKey: .sessionId),
                    peer: try c.decode(DeviceInfo.self, forKey: .peer),
                    videoCodec: try c.decodeIfPresent(VideoCodec.self, forKey: .videoCodec) ?? .h265)
            case "session_rejected":
                self = .sessionRejected(reason: try c.decode(String.self, forKey: .reason))
            case "rtc_signal":
                self = .rtcSignal(
                    sessionId: try c.decode(String.self, forKey: .sessionId),
                    signal: try c.decode(RtcSignal.self, forKey: .signal))
            case "session_control":
                self = .sessionControl(
                    sessionId: try c.decode(String.self, forKey: .sessionId),
                    control: try c.decode(SessionControl.self, forKey: .control))
            case "session_ended":
                self = .sessionEnded(
                    sessionId: try c.decode(String.self, forKey: .sessionId),
                    reason: try c.decode(ByeReason.self, forKey: .reason),
                    detail: try c.decodeIfPresent(String.self, forKey: .detail))
            case "pong":
                self = .pong
            case "error":
                self = .error(message: try c.decode(String.self, forKey: .message))
            case let other:
                throw DecodingError.dataCorruptedError(
                    forKey: .type, in: c, debugDescription: "unknown message \(other)")
            }
        }

        static func decode(_ text: String) throws -> ServerMessage {
            try Wire.decoder.decode(ServerMessage.self, from: Data(text.utf8))
        }
    }

    // MARK: - Coders

    static let encoder = JSONEncoder()
    static let decoder = JSONDecoder()
}
