// C interface to the Waystone transport, implemented in Rust.
//
// Mirror of `crates/waystone-rtc/src/ffi.rs` and `client-ios/Waystone/Stream/waystone_rtc.h`.

#ifndef WAYSTONE_RTC_H
#define WAYSTONE_RTC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    WS_EVENT_CONTROL = 0,
    WS_EVENT_VIDEO = 1,
    WS_EVENT_AUDIO = 2,
    WS_EVENT_INPUT = 3,
} ws_event_kind;

typedef void (*ws_event_callback)(int32_t kind, const uint8_t *data, size_t len, void *ctx);

typedef struct WsEngine WsEngine;

WsEngine *ws_engine_start(const char *config_json,
                          ws_event_callback callback,
                          void *ctx,
                          char *error_out,
                          size_t error_len);

void ws_engine_stop(WsEngine *handle);
size_t ws_engine_local_port(WsEngine *handle, char *buffer, size_t len);
void ws_engine_offer(WsEngine *handle);
void ws_engine_accept_offer(WsEngine *handle, const char *sdp);
void ws_engine_accept_answer(WsEngine *handle, const char *sdp);
void ws_engine_add_candidate(WsEngine *handle, const char *candidate);
void ws_engine_send_video(WsEngine *handle, const uint8_t *data, size_t len, uint64_t rtp_time);
void ws_engine_send_audio(WsEngine *handle, const uint8_t *data, size_t len, uint64_t rtp_time);
void ws_engine_send_input(WsEngine *handle, bool reliable, const uint8_t *data, size_t len);
void ws_engine_set_video_bitrate(WsEngine *handle, uint32_t bits_per_second);
uint32_t ws_abi_version(void);

#ifdef __cplusplus
}
#endif

#endif
