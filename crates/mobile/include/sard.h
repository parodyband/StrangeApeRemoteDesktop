// Sard core for the iPhone app: the C API of crates/mobile (libsard_mobile.a).
//
// Threading: functions may be called from any thread unless noted. Callbacks
// arrive on the core's own threads; hop to the main queue ASYNCHRONOUSLY
// (DispatchQueue.main.async). Never block a callback on the main thread:
// sard_session_close waits for the threads that run callbacks.
//
// Strings returned as `char *` are JSON, owned by the caller: free them with
// sard_free.

#ifndef SARD_H
#define SARD_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// ---------------------------------------------------------------- setup

// Once at launch: where to keep identity and paired computers (Application
// Support), and the name other computers see for this phone.
void sard_init(const char *config_dir, const char *device_name);
void sard_set_device_name(const char *name);
void sard_free(char *s);

// TCP + TLS for Connect Anywhere (the app implements it on Network.framework).
// Blocking calls on a background thread; timeouts in ms (-1: forever).
typedef struct {
    // Connect; NULL on failure.
    void *(*open)(const char *host, uint16_t port, bool tls, int32_t timeout_ms);
    // Send all bytes: len on success, -1 error, -2 timeout.
    intptr_t (*write)(void *conn, const uint8_t *data, size_t len, int32_t timeout_ms);
    // Receive up to len bytes: count, 0 at end of stream, -1 error, -2 timeout.
    intptr_t (*read)(void *conn, uint8_t *buf, size_t len, int32_t timeout_ms);
    void (*close)(void *conn);
} SardTransport;
void sard_set_transport(SardTransport t);

// {"fingerprint": "abcd-...", "version": "0.1.19"}
char *sard_identity(void);
// [{"key": hex, "name": "...", "addr": "192.168.1.5", "fingerprint": "..."}]
char *sard_hosts(void);
// Pair using the 6-digit code the computer shows. BLOCKS up to ~10 s.
// {"ok": true, "key": hex, "name": "..."} or {"ok": false, "error": "..."}
char *sard_pair(const char *addr, const char *code);
// A scanned pairing QR code (`sard://pair?...`): {"ok", "name", "addrs"} or
// {"ok": false, "error"}. sard_pair_link pairs from it (BLOCKS up to ~12 s;
// tries each address; accepts only the computer whose key the code names).
char *sard_parse_pair_link(const char *link);
char *sard_pair_link(const char *link);
bool sard_forget(const char *key);
bool sard_set_address(const char *key, const char *addr);
// Connect Anywhere: {"enabled": bool}; set the invite code ("" turns it off).
char *sard_anywhere(void);
char *sard_set_anywhere(const char *token);

// evdev key code for a USB HID usage (UIKey.keyCode.rawValue); 0 if none.
uint16_t sard_hid_to_evdev(uint32_t usage);

// --------------------------------------------------------------- sessions

typedef struct SardSession SardSession;

typedef struct {
    void *ctx;
    // Events (JSON, "type" field):
    //   connected {name, via: "local network" | "internet"}   failed {error}
    //   disconnected {reason}   status {text}   audio {}
    //   stream {width, height, fps, codec, host_platform, host_name, cursor_metadata}
    //   video {width, height}   (decoded size changed)
    //   displays {current, list: [{id, name, width, height}]}
    //   stats {fps, mbps, latency_ms, decode_ms, rtt_ms}   clipboard {text}
    //   shells {list: [{id, name, attached, age_s}]}
    //   shell_opened {token, id, name, resumed}   shell_refused {token, reason}
    //   shell_exit {id, message}
    void (*event)(void *ctx, const char *json);
    // Terminal output for shell `id`. Copy it; call sard_shell_ack(id, 1) once
    // it has been fed to the terminal (flow control: the host pauses when too
    // much is unacknowledged).
    void (*shell_data)(void *ctx, uint32_t id, const uint8_t *data, size_t len);
    // Host cursor image (RGBA8, straight alpha, width*height*4 bytes).
    void (*cursor_shape)(void *ctx, uint32_t serial, uint16_t width, uint16_t height,
                         uint16_t hot_x, uint16_t hot_y, const uint8_t *rgba);
    // Host cursor position, 0..1 of the video, and whether it's visible.
    void (*cursor_pos)(void *ctx, float x, float y, bool visible);
} SardCallbacks;

typedef struct {
    uint16_t max_width, max_height; // largest video wanted (0: 2560x1600)
    uint16_t max_fps;               // 0: 60
    uint16_t fit_width, fit_height; // fit the host's screen at start (0: don't)
    bool audio;
} SardScreenOptions;

#define SARD_MODE_SCREEN 0
#define SARD_MODE_TERMINAL 1

// Connect to a paired computer (key from sard_hosts). Returns at once;
// progress comes as `connected` / `failed` events. A screen and a terminal
// session to the same computer can be open at the same time.
SardSession *sard_connect(const char *key, uint32_t mode, SardScreenOptions opts, SardCallbacks cb);
// Disconnect and free. After it returns no callback runs. Don't call it
// from inside a callback.
void sard_session_close(SardSession *s);

// Screen sessions. MAIN THREAD: show the video in this CAMetalLayer
// (its drawableSize becomes the video size), or NULL to stop drawing.
void sard_session_set_layer(SardSession *s, void *metal_layer);
void sard_mouse_move(SardSession *s, float x, float y);   // 0..1 of the video
void sard_mouse_button(SardSession *s, uint8_t button, bool down, float x, float y); // 1 L, 2 R, 3 M
void sard_scroll(SardSession *s, float dx, float dy);      // logical pixels
void sard_key(SardSession *s, uint16_t evdev_code, bool down);
// Type text as US-layout key presses, holding `modifiers` (evdev codes).
void sard_type_text(SardSession *s, const char *utf8, const uint16_t *modifiers, size_t n_modifiers);
void sard_release_all(SardSession *s);
void sard_select_display(SardSession *s, uint32_t id);
// Fit the host's screen to about width x height logical pixels (0, 0: restore).
void sard_fit_display(SardSession *s, uint16_t width, uint16_t height);
void sard_request_keyframe(SardSession *s);
void sard_set_clipboard(SardSession *s, const char *text);
// Audio thread: frames of interleaved stereo int16 at 48 kHz (silence if none).
void sard_audio_pull(SardSession *s, int16_t *out, size_t frames);

// Terminal sessions.
void sard_shell_open(SardSession *s, uint32_t token, uint32_t id, uint16_t cols, uint16_t rows); // id 0: new
void sard_shell_write(SardSession *s, uint32_t id, const uint8_t *data, size_t len);
void sard_shell_resize(SardSession *s, uint32_t id, uint16_t cols, uint16_t rows);
void sard_shell_ack(SardSession *s, uint32_t id, uint32_t count);
void sard_shell_close(SardSession *s, uint32_t id);
void sard_shell_list(SardSession *s);

#ifdef __cplusplus
}
#endif

#endif
