// C interface to the Waystone transport, implemented in Rust.
//
// The library is built from `crates/waystone-rtc`; see
// `client-ios/scripts/build-rust.sh`. Every function is thread safe unless noted.

#ifndef WAYSTONE_RTC_H
#define WAYSTONE_RTC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// Event kinds delivered to `ws_event_callback`.
typedef enum {
    /// `data` is a UTF-8 JSON object with a `kind` field.
    WS_EVENT_CONTROL = 0,
    /// `data` is a media frame: 16 byte header plus Annex-B HEVC payload.
    WS_EVENT_VIDEO = 1,
    /// `data` is a media frame: 16 byte header plus one Opus packet.
    WS_EVENT_AUDIO = 2,
    /// `data` is a media frame: 16 byte header plus an input frame.
    WS_EVENT_INPUT = 3,
} ws_event_kind;

/// Receives engine events.
///
/// `data` is only valid for the duration of the call. `ctx` is the pointer that
/// was passed to `ws_engine_start`. Calls are serialized; the callback may call
/// any `ws_engine_*` function, including `ws_engine_stop`.
typedef void (*ws_event_callback)(int32_t kind, const uint8_t *data, size_t len, void *ctx);

/// Opaque engine handle.
typedef struct WsEngine WsEngine;

/// Starts an engine. `config_json` is a UTF-8 JSON object:
///
/// ```json
/// {
///   "role": "host" | "controller",
///   "viewport": {"width": 2556, "height": 1179, "dpi": 460, "fps": 60},
///   "audio": true,
///   "stun": "home.example.com:3478",
///   "turn": {"server": "home.example.com:3478", "username": "u", "password": "p",
///            "relay_bitrate_ceiling": 4000000},
///   "bind": "0.0.0.0:0",
///   "video_codec": "h265",
///   "relay_only": false
/// }
/// ```
///
/// `relay_only` (optional, default false) gathers and advertises only the TURN
/// relay candidate; the session closes if the allocation fails. The engine
/// does not clamp the encoder itself: the `path_update` control event carries
/// `"relayed": true` when the selected pair uses a relay candidate on either
/// side, and the application caps its bitrate at `relay_bitrate_ceiling`.
///
/// Returns NULL on failure and writes a message into `error_out`.
WsEngine *ws_engine_start(const char *config_json,
                          ws_event_callback callback,
                          void *ctx,
                          char *error_out,
                          size_t error_len);

/// Stops the engine and releases the handle.
///
/// After this returns the callback is never invoked again, so `ctx` may be
/// freed; no `closed` event follows a stop. It is safe to call from inside the
/// callback, in which case the remaining teardown finishes on a background
/// thread.
void ws_engine_stop(WsEngine *handle);

/// Writes the local UDP port into `buffer` and returns the number of bytes used.
size_t ws_engine_local_port(WsEngine *handle, char *buffer, size_t len);

/// Creates the SDP offer (controller only).
void ws_engine_offer(WsEngine *handle);

/// Accepts a remote offer (host only).
void ws_engine_accept_offer(WsEngine *handle, const char *sdp);

/// Applies a remote answer (controller only).
void ws_engine_accept_answer(WsEngine *handle, const char *sdp);

/// Adds a trickled remote ICE candidate.
void ws_engine_add_candidate(WsEngine *handle, const char *candidate);

/// Writes one Annex-B HEVC frame. When the engine falls behind it drops the
/// oldest queued frame and reports a `frame_dropped` control event.
void ws_engine_send_video(WsEngine *handle, const uint8_t *data, size_t len, uint64_t rtp_time);

/// Writes one Opus packet with a 48 kHz timestamp.
void ws_engine_send_audio(WsEngine *handle, const uint8_t *data, size_t len, uint64_t rtp_time);

/// Sends one input frame on the reliable or the fast channel. Audio packets and
/// input frames are dropped when their bounded queue is full.
void ws_engine_send_input(WsEngine *handle, bool reliable, const uint8_t *data, size_t len);

/// Caps the outgoing video bitrate in bits per second.
void ws_engine_set_video_bitrate(WsEngine *handle, uint32_t bits_per_second);

/// ABI version, bumped when the callback contract changes.
uint32_t ws_abi_version(void);

#ifdef __cplusplus
}
#endif

#endif
