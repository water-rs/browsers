#ifndef WATERUI_WPE_H
#define WATERUI_WPE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WATER_WPE_ABI_VERSION 4
#define WATER_WPE_MAX_PLANES 4

/* Which kind of platform buffer a frame carries.
 *
 * WPE Platform decides this, not this bridge: a display with a DRM render node
 * renders into a `WPEBufferDMABuf`, and one without renders into a
 * `WPEBufferSHM`. Both are ordinary platform buffers — a host with no render
 * node (a container, a virtual machine, a hosted CI runner) simply produces the
 * second kind. */
enum {
    WATER_WPE_FRAME_DMA_BUF = 0,
    WATER_WPE_FRAME_SHM = 1
};

typedef struct WaterWpeRuntime WaterWpeRuntime;
typedef struct WaterWpePage WaterWpePage;

typedef struct {
    const uint8_t *data;
    size_t len;
    void *user_data;
    void (*destroy)(void *user_data);
} WaterWpeBytes;

/* One frame of page output, tagged by `kind`.
 *
 * The fields are grouped so the layout has no padding on either side of the
 * ABI, and each is documented with the kind it belongs to; a field belonging to
 * the other kind is zero (`-1` for a descriptor). `token` and `width`/`height`
 * are common to both.
 *
 * `data` points into the buffer the token holds a reference to, and stays valid
 * until `water_wpe_frame_release` is called for that token. */
typedef struct {
    void *token;
    /* SHM: first byte of the top row. */
    const uint8_t *data;
    /* SHM: bytes `data` addresses. */
    size_t len;
    /* DMA-BUF: DRM format modifier. */
    uint64_t modifier;
    /* WATER_WPE_FRAME_DMA_BUF or WATER_WPE_FRAME_SHM. */
    uint32_t kind;
    uint32_t width;
    uint32_t height;
    /* DMA-BUF: DRM fourcc. */
    uint32_t format;
    /* SHM: WPEPixelFormat. */
    uint32_t pixel_format;
    /* SHM: bytes between adjacent rows. */
    uint32_t stride;
    /* DMA-BUF: planes described by the three arrays below. */
    uint32_t n_planes;
    /* DMA-BUF: the buffer's rendering fence, or -1 when it has none. */
    int rendering_fence_fd;
    int fds[WATER_WPE_MAX_PLANES];
    uint32_t offsets[WATER_WPE_MAX_PLANES];
    uint32_t strides[WATER_WPE_MAX_PLANES];
} WaterWpeFrame;

typedef void (*WaterWpeDestroyNotify)(void *user_data);
typedef void (*WaterWpeEventCallback)(
    void *user_data,
    uint32_t kind,
    const char *first,
    const char *second,
    double number);
typedef void (*WaterWpeFrameCallback)(void *user_data, const WaterWpeFrame *frame);
/* Receives one `waterui.invoke(...)` envelope exactly as the page sent it, and
 * returns the JavaScript that completes the call. The envelope format belongs to
 * `waterui_webview::bridge`; this layer only transports it.
 *
 * `origin` is the calling document's `scheme://host[:port]`, or the empty string
 * when it has none to report — an opaque origin, or a page that has not
 * committed a document yet. It is authenticated by the engine rather than taken
 * from the envelope, which page script writes. */
typedef WaterWpeBytes (*WaterWpeMessageCallback)(
    void *user_data,
    const char *origin,
    const char *envelope);
typedef void (*WaterWpeResultCallback)(
    void *user_data,
    bool success,
    const char *data,
    size_t len);

/* One descriptor the runtime's main context wants watched, in `poll(2)` terms:
 * `events` is a mask of `POLLIN` and friends, taken from GLib unchanged. */
typedef struct {
    int fd;
    int16_t events;
} WaterWpePollFd;

/* When the runtime's main context next has work to do.
 *
 * `ready` means a source can be dispatched right now and the host must call
 * `water_wpe_runtime_iteration` without waiting at all.
 *
 * `timeout_ms` is how long until the earliest timer source is due, or -1 when no
 * source has a timeout and only a descriptor can wake the context. It says
 * nothing about the descriptors: a host that does not watch them has to look
 * again on a bound of its own, and a host that folds them into its own wakeup
 * does not. */
typedef struct {
    bool ready;
    int32_t timeout_ms;
} WaterWpeReadiness;

uint32_t water_wpe_abi_version(void);
WaterWpeRuntime *water_wpe_runtime_new(char **error);
void water_wpe_runtime_free(WaterWpeRuntime *runtime);
bool water_wpe_runtime_iteration(WaterWpeRuntime *runtime);
/* Reports what the runtime's main context is waiting for, so a host can schedule
 * the next `water_wpe_runtime_iteration` instead of guessing at an interval.
 *
 * This is one `g_main_context_prepare` / `g_main_context_query` pass: it asks
 * every source when it next wants to run and dispatches nothing. Writes the
 * readiness through `readiness`, fills up to `capacity` entries of `fds`, and
 * returns how many descriptors the context actually has — which may exceed
 * `capacity`, in which case none were written and the caller retries with a
 * buffer that size, exactly as `g_main_context_query` is used. `fds` may be NULL
 * only when `capacity` is zero. */
uint32_t water_wpe_runtime_readiness(
    WaterWpeRuntime *runtime,
    WaterWpeReadiness *readiness,
    WaterWpePollFd *fds,
    uint32_t capacity);
void water_wpe_string_free(char *string);

WaterWpePage *water_wpe_page_new(
    WaterWpeRuntime *runtime,
    WaterWpeEventCallback event_callback,
    WaterWpeFrameCallback frame_callback,
    WaterWpeMessageCallback message_callback,
    void *user_data,
    WaterWpeDestroyNotify destroy_user_data,
    char **error);
void water_wpe_page_free(WaterWpePage *page);
void water_wpe_page_load_uri(WaterWpePage *page, const char *uri);
void water_wpe_page_go_back(WaterWpePage *page);
void water_wpe_page_go_forward(WaterWpePage *page);
void water_wpe_page_stop(WaterWpePage *page);
void water_wpe_page_reload(WaterWpePage *page);
bool water_wpe_page_can_go_back(WaterWpePage *page);
bool water_wpe_page_can_go_forward(WaterWpePage *page);
void water_wpe_page_set_redirects_enabled(WaterWpePage *page, bool enabled);
void water_wpe_page_set_user_agent(WaterWpePage *page, const char *user_agent);
void water_wpe_page_resize(
    WaterWpePage *page,
    uint32_t width,
    uint32_t height,
    double scale);
void water_wpe_page_set_focus(WaterWpePage *page, bool focused);
void water_wpe_page_pointer_button(
    WaterWpePage *page,
    bool pressed,
    uint32_t button,
    double x,
    double y,
    uint32_t modifiers,
    uint32_t time_ms);
void water_wpe_page_pointer_move(
    WaterWpePage *page,
    double x,
    double y,
    double delta_x,
    double delta_y,
    uint32_t modifiers,
    uint32_t time_ms);
void water_wpe_page_scroll(
    WaterWpePage *page,
    double x,
    double y,
    double delta_x,
    double delta_y,
    bool precise,
    bool stopped,
    uint32_t modifiers,
    uint32_t time_ms);
void water_wpe_page_key(
    WaterWpePage *page,
    bool pressed,
    uint32_t keycode,
    uint32_t keyval,
    uint32_t modifiers,
    uint32_t time_ms);
/* Evaluates `script` and discards its result. Used to settle a page promise
 * after an asynchronous handler has finished. */
void water_wpe_page_evaluate(WaterWpePage *page, const char *script);
/* Installs a document script under `key`, replacing whatever script that key
 * already names. The scripts are injected into the top frame only: the bridge is
 * a capability, and embedding a document does not grant it one. */
void water_wpe_page_add_script(
    WaterWpePage *page,
    const char *key,
    const char *script,
    uint32_t injection_time);
void water_wpe_page_set_cookie(WaterWpePage *page, const char *cookie);
void water_wpe_page_get_cookies(
    WaterWpePage *page,
    WaterWpeResultCallback callback,
    void *user_data);
/* Evaluates `script` as a program and reports its result. A promise comes back
 * as a promise: this is the raw path, so nothing is awaited. */
void water_wpe_page_run_javascript(
    WaterWpePage *page,
    const char *script,
    WaterWpeResultCallback callback,
    void *user_data);
/* Runs `body` as the body of an async function and reports the value its promise
 * resolves with. Every typed evaluation takes this path, because the shared
 * `__wateruiEval` wrapper is async and `webkit_web_view_evaluate_javascript`
 * would hand back the unresolved promise instead of the envelope. */
void water_wpe_page_call_async_javascript(
    WaterWpePage *page,
    const char *body,
    WaterWpeResultCallback callback,
    void *user_data);

void water_wpe_frame_presented(void *token);
void water_wpe_frame_release(void *token, int release_fence_fd);

#ifdef __cplusplus
}
#endif

#endif
