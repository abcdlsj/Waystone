import Foundation

/// Wire protocol mirror of `crates/waystone-proto`.
///
/// The Rust side uses serde with an internally tagged enum (`type`), so decoding
/// here goes through `JSONSerialization`: the schema is small and explicit, and a
/// mismatch fails loudly in one place instead of in a pile of `Codable` errors.
enum Wire {

    // MARK: - Shared

    struct Viewport {
        var width: UInt32
        var height: UInt32
        var dpi: UInt16
        var fps: UInt32

        static func current(screen: CGRect, scale: CGFloat, fps: UInt32) -> Viewport {
            Viewport(
                width: UInt32((screen.width * scale).rounded()),
                height: UInt32((screen.height * scale).rounded()),
                dpi: 460,
                fps: fps
            )
        }
    }

    struct DeviceInfo {
        var id: String
        var name: String
        var platform: String
        var role: String
        var publicKey: String

        var json: [String: Any] {
            [
                "id": id,
                "name": name,
                "platform": platform,
                "role": role,
                "public_key": publicKey,
            ]
        }

        init(id: String, name: String, platform: String, role: String, publicKey: String) {
            self.id = id
            self.name = name
            self.platform = platform
            self.role = role
            self.publicKey = publicKey
        }

        init?(json: [String: Any]) {
            guard let id = json["id"] as? String,
                let name = json["name"] as? String,
                let platform = json["platform"] as? String,
                let role = json["role"] as? String,
                let publicKey = json["public_key"] as? String
            else { return nil }
            self.init(id: id, name: name, platform: platform, role: role, publicKey: publicKey)
        }
    }

    struct TurnCredentials {
        var server: String
        var username: String
        var password: String

        init?(json: Any?) {
            guard let json = json as? [String: Any],
                let server = json["server"] as? String,
                let username = json["username"] as? String,
                let password = json["password"] as? String
            else { return nil }
            self.server = server
            self.username = username
            self.password = password
        }
    }

    struct PeerState {
        var device: DeviceInfo
        var online: Bool
        var rttMs: UInt32?
    }

    struct Challenge {
        var nonce: String
        var serverId: String

        /// Must match `waystone_proto::identity::Challenge::signing_payload`.
        func signingPayload() -> Data {
            Data("waystone-auth-v1|\(serverId)|\(nonce)".utf8)
        }
    }

    // MARK: - Server to client

    enum ServerMessage {
        case challenge(nonce: String, serverId: String, protocolVersion: UInt32)
        case pairAccepted(serverId: String)
        case pairRejected(reason: String)
        case authOk(peers: [PeerState], turn: TurnCredentials?)
        case authRejected(reason: String)
        case peerUpdate(peers: [PeerState])
        case sessionAccepted(sessionId: String, isHost: Bool, peer: DeviceInfo, audio: Bool)
        case sessionRejected(reason: String)
        case rtcSignal(sessionId: String, signal: RtcSignal)
        case sessionControl(sessionId: String, control: SessionControl)
        case sessionEnded(sessionId: String, reason: String, detail: String?)
        case pong(nonce: UInt64)
        case error(message: String)

        static func decode(_ text: String) -> ServerMessage? {
            guard let data = text.data(using: .utf8),
                let object = try? JSONSerialization.jsonObject(with: data),
                let json = object as? [String: Any],
                let type = json["type"] as? String
            else { return nil }

            switch type {
            case "challenge":
                guard let nonce = json["nonce"] as? String,
                    let serverId = json["server_id"] as? String
                else { return nil }
                return .challenge(
                    nonce: nonce,
                    serverId: serverId,
                    protocolVersion: (json["protocol_version"] as? NSNumber)?.uint32Value ?? 0)
            case "pair_accepted":
                return .pairAccepted(serverId: json["server_id"] as? String ?? "")
            case "pair_rejected":
                return .pairRejected(reason: json["reason"] as? String ?? "")
            case "auth_ok":
                return .authOk(
                    peers: decodePeers(json["peers"]),
                    turn: TurnCredentials(json: json["turn"]))
            case "auth_rejected":
                return .authRejected(reason: json["reason"] as? String ?? "")
            case "peer_update":
                return .peerUpdate(peers: decodePeers(json["peers"]))
            case "session_accepted":
                guard let sessionId = json["session_id"] as? String,
                    let peerJson = json["peer"] as? [String: Any],
                    let peer = DeviceInfo(json: peerJson)
                else { return nil }
                return .sessionAccepted(
                    sessionId: sessionId,
                    isHost: json["is_host"] as? Bool ?? false,
                    peer: peer,
                    audio: json["audio"] as? Bool ?? true)
            case "session_rejected":
                return .sessionRejected(reason: json["reason"] as? String ?? "")
            case "rtc_signal":
                guard let sessionId = json["session_id"] as? String,
                    let signalJson = json["signal"] as? [String: Any],
                    let signal = RtcSignal(json: signalJson)
                else { return nil }
                return .rtcSignal(sessionId: sessionId, signal: signal)
            case "session_control":
                guard let sessionId = json["session_id"] as? String,
                    let controlJson = json["control"] as? [String: Any],
                    let control = SessionControl(json: controlJson)
                else { return nil }
                return .sessionControl(sessionId: sessionId, control: control)
            case "session_ended":
                guard let sessionId = json["session_id"] as? String else { return nil }
                return .sessionEnded(
                    sessionId: sessionId,
                    reason: json["reason"] as? String ?? "network_timeout",
                    detail: json["detail"] as? String)
            case "pong":
                return .pong(nonce: (json["nonce"] as? NSNumber)?.uint64Value ?? 0)
            case "error":
                return .error(message: json["message"] as? String ?? "")
            default:
                return nil
            }
        }

        private static func decodePeers(_ value: Any?) -> [PeerState] {
            guard let list = value as? [[String: Any]] else { return [] }
            return list.compactMap { entry in
                guard let deviceJson = entry["device"] as? [String: Any],
                    let device = DeviceInfo(json: deviceJson)
                else { return nil }
                return PeerState(
                    device: device,
                    online: entry["online"] as? Bool ?? false,
                    rttMs: (entry["rtt_ms"] as? NSNumber)?.uint32Value)
            }
        }
    }

    enum RtcSignal {
        case offer(sdp: String)
        case answer(sdp: String)
        case ice(candidate: String)
        case iceComplete

        init?(json: [String: Any]) {
            switch json["kind"] as? String {
            case "offer":
                guard let sdp = json["sdp"] as? String else { return nil }
                self = .offer(sdp: sdp)
            case "answer":
                guard let sdp = json["sdp"] as? String else { return nil }
                self = .answer(sdp: sdp)
            case "ice":
                guard let candidate = json["candidate"] as? String else { return nil }
                self = .ice(candidate: candidate)
            case "ice_complete":
                self = .iceComplete
            default:
                return nil
            }
        }

        var json: [String: Any] {
            switch self {
            case .offer(let sdp): return ["kind": "offer", "sdp": sdp]
            case .answer(let sdp): return ["kind": "answer", "sdp": sdp]
            case .ice(let candidate):
                return ["kind": "ice", "candidate": candidate]
            case .iceComplete: return ["kind": "ice_complete"]
            }
        }
    }

    enum SessionControl {
        case setFrameRate(fps: UInt32)
        case requestKeyframe
        case setVideoEnabled(enabled: Bool)
        case setAudioEnabled(enabled: Bool)
        case selectDisplay(index: UInt32)

        init?(json: [String: Any]) {
            switch json["kind"] as? String {
            case "set_frame_rate":
                self = .setFrameRate(fps: (json["fps"] as? NSNumber)?.uint32Value ?? 60)
            case "request_keyframe":
                self = .requestKeyframe
            case "set_video_enabled":
                self = .setVideoEnabled(enabled: json["enabled"] as? Bool ?? true)
            case "set_audio_enabled":
                self = .setAudioEnabled(enabled: json["enabled"] as? Bool ?? true)
            case "select_display":
                self = .selectDisplay(index: (json["index"] as? NSNumber)?.uint32Value ?? 0)
            default:
                return nil
            }
        }

        var json: [String: Any] {
            switch self {
            case .setFrameRate(let fps): return ["kind": "set_frame_rate", "fps": fps]
            case .requestKeyframe: return ["kind": "request_keyframe"]
            case .setVideoEnabled(let enabled):
                return ["kind": "set_video_enabled", "enabled": enabled]
            case .setAudioEnabled(let enabled):
                return ["kind": "set_audio_enabled", "enabled": enabled]
            case .selectDisplay(let index):
                return ["kind": "select_display", "index": index]
            }
        }
    }

    // MARK: - Client to server

    enum ClientMessage {
        case pairRequest(token: String, device: DeviceInfo)
        case auth(identityId: String, publicKey: String, signature: String, name: String)
        case sessionStart(target: String, viewport: Viewport, audio: Bool)
        case rtcSignal(sessionId: String, signal: RtcSignal)
        case sessionControl(sessionId: String, control: SessionControl)
        case sessionBye(sessionId: String, reason: String)
        case ping(nonce: UInt64)

        var json: [String: Any] {
            switch self {
            case .pairRequest(let token, let device):
                return ["type": "pair_request", "token": token, "device": device.json]
            case .auth(let identityId, let publicKey, let signature, let name):
                return [
                    "type": "auth",
                    "identity": ["id": identityId, "public_key": publicKey],
                    "signature": signature,
                    "protocol_version": 1,
                    "name": name,
                ]
            case .sessionStart(let target, let viewport, let audio):
                return [
                    "type": "session_start",
                    "target": target,
                    "viewport": [
                        "width": viewport.width,
                        "height": viewport.height,
                        "dpi": viewport.dpi,
                        "fps": viewport.fps,
                    ],
                    "audio": audio,
                ]
            case .rtcSignal(let sessionId, let signal):
                return [
                    "type": "rtc_signal", "session_id": sessionId, "signal": signal.json,
                ]
            case .sessionControl(let sessionId, let control):
                return [
                    "type": "session_control", "session_id": sessionId, "control": control.json,
                ]
            case .sessionBye(let sessionId, let reason):
                return ["type": "session_bye", "session_id": sessionId, "reason": reason]
            case .ping(let nonce):
                return ["type": "ping", "nonce": nonce]
            }
        }

        func encoded() -> String {
            let data = try? JSONSerialization.data(withJSONObject: json)
            return data.flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
        }
    }
}
