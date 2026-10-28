#ifndef WATERUI_WPE_H
#define WATERUI_WPE_H

#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WATER_WPE_ABI_VERSION 6
#ifdef WATER_WPE_EXPECTED_ABI_VERSION
static_assert(WATER_WPE_ABI_VERSION == WATER_WPE_EXPECTED_ABI_VERSION,
    "WPE bridge ABI disagrees with the resolved browser package");
#endif
#define WATER_WPE_MAX_PLANES 4

typedef struct WaterWpeRuntime WaterWpeRuntime;
typedef struct WaterWpePage WaterWpePage;
typedef struct WaterWpeReply WaterWpeReply;

typedef struct {
    const uint8_t *data;
    size_t len;
    void *user_data;
    void (*destroy)(void *user_data);
} WaterWpeBytes;

typedef enum {
    WATER_WPE_FRAME_DMA_BUF = 0,
    WATER_WPE_FRAME_SHM = 1,
} WaterWpeFrameKind;

typedef struct {
    void *token;
    const uint8_t *data;
    size_t len;
    uint64_t modifier;
    uint32_t kind;
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint32_t pixel_format;
    uint32_t stride;
    uint32_t n_planes;
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
/* Receives the calling document's authenticated origin and one bridge envelope.
 * The callback owns `reply` and must return a result through the reply API. */
typedef void (*WaterWpeMessageCallback)(
    void *user_data,
    WaterWpeReply *reply,
    const char *origin,
    const char *envelope);
typedef void (*WaterWpeResultCallback)(
    void *user_data,
    bool success,
    const char *data,
    size_t len);

/* One answer to a `waterui://localhost` request. `headers` is `"Name: value"`
 * lines joined by `\n`; each `WaterWpeBytes` releases its own storage through
 * `destroy`. */
typedef struct {
    uint32_t status;
    WaterWpeBytes headers;
    WaterWpeBytes body;
} WaterWpeAssetResponse;
/* Answers one request on the page's asset origin. `method` and `uri` are the
 * engine's own authenticated request — the URI arrives whole so the Rust side
 * applies the shared host, method and traversal rules to exactly what the
 * engine was asked to load. */
typedef WaterWpeAssetResponse (*WaterWpeAssetCallback)(
    void *user_data,
    const char *method,
    const char *uri);

/* Answers one page-initiated navigation decision synchronously. `url` is the
 * navigation's target, `from` the committed document it leaves (NULL before
 * the first commit), `is_main_frame` whether the action targets the top-level
 * frame, and `user_gesture` what `webkit_navigation_action_is_user_gesture`
 * reports. Returns whether the engine proceeds: TRUE allows the navigation,
 * FALSE ignores it. Redirects, the engine's own response decisions and
 * application-directed loads — the navigations `water_wpe_page_load_uri`,
 * `water_wpe_page_reload`, `water_wpe_page_go_back` and
 * `water_wpe_page_go_forward` issue — never reach this callback. */
typedef bool (*WaterWpePolicyCallback)(
    void *user_data,
    const char *url,
    const char *from,
    bool is_main_frame,
    bool user_gesture);

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
/* Arms the page's `waterui://localhost` asset origin: the scheme is registered
 * on the page's web context (once per context) and `callback` answers the
 * page's requests on it. Call between `water_wpe_page_new` and the first
 * `water_wpe_page_load_uri` — an asset URL can only be navigated to after the
 * interception exists. `user_data` is freed by `destroy` when the page dies. */
void water_wpe_page_set_asset_server(
    WaterWpePage *page,
    WaterWpeAssetCallback callback,
    void *user_data,
    WaterWpeDestroyNotify destroy);
/* Arms the page's navigation policy: `callback` answers each page-initiated
 * navigation action before the engine commits to it, returning FALSE to keep
 * the engine from proceeding. Pass NULL to disarm; `user_data` is freed by
 * `destroy` when the page dies. Navigations the bridge's own entry points
 * issue are marked internally, like WebKit's `pendingAPIRequestURL`, and the
 * engine's decision bypasses the callback for them. */
void water_wpe_page_set_navigation_policy(
    WaterWpePage *page,
    WaterWpePolicyCallback callback,
    void *user_data,
    WaterWpeDestroyNotify destroy);
void water_wpe_page_set_bridge_origins(WaterWpePage *page, const char *wire);
void water_wpe_page_get_web_process_identifier(
    WaterWpePage *page,
    WaterWpeResultCallback callback,
    void *user_data);
void water_wpe_reply_return(WaterWpeReply *reply, const char *json, size_t len);
void water_wpe_reply_free(WaterWpeReply *reply);
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
/* Evaluates `script` as a program and reports the string it evaluates to; a
 * value of any other kind is reported as a failure. A promise is not awaited:
 * this is the raw path. */
void water_wpe_page_run_javascript(
    WaterWpePage *page,
    const char *script,
    WaterWpeResultCallback callback,
    void *user_data);
/* Runs `body` as the body of an async function and reports the string its
 * promise resolves with; a value of any other kind is reported as a failure. Every typed evaluation takes this path, because the shared
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
