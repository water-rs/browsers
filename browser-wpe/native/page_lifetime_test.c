/* Whitebox regression for the page-callback lifetime fix: the fixture
 * includes the bridge source so it can inspect private state the public ABI
 * never exposes. It never runs in an application — the CMake option builds
 * it as a separate shared object the Rust test loads. */

#include "waterui_wpe.c"

typedef struct {
    unsigned destroy_count;
} DestroyCounter;

/* The first server's teardown state: the page it lives on, its own counter
 * and the counter the nested server it installs counts into. */
typedef struct {
    WaterWpePage *page;
    DestroyCounter *own;
    DestroyCounter *nested;
} ReenteringDestroy;

static void counted_destroy(void *user_data)
{
    DestroyCounter *counter = user_data;
    ++counter->destroy_count;
}

static void reentering_destroy(void *user_data);

static WaterWpeAssetResponse fake_asset_callback(
    void *user_data,
    const char *method,
    const char *uri)
{
    (void)user_data;
    (void)method;
    (void)uri;
    WaterWpeAssetResponse response = { 200, { 0 }, { 0 } };
    return response;
}

static void fake_event(
    void *user_data,
    uint32_t kind,
    const char *first,
    const char *second,
    double number)
{
    (void)user_data;
    (void)kind;
    (void)first;
    (void)second;
    (void)number;
}

/* The frame callback owns the token and the duplicated plane FDs: release
 * the token through the contract and close every descriptor it carried. */
static void fake_frame(void *user_data, const WaterWpeFrame *frame)
{
    (void)user_data;
    water_wpe_frame_release(frame->token, frame->rendering_fence_fd);
    for (uint32_t i = 0; i < frame->n_planes; ++i) {
        if (frame->fds[i] >= 0)
            close(frame->fds[i]);
    }
}

/* The message callback owns the reply and must free it. */
static void fake_message(
    void *user_data,
    WaterWpeReply *reply,
    const char *origin,
    const char *envelope)
{
    (void)user_data;
    (void)origin;
    (void)envelope;
    water_wpe_reply_free(reply);
}

static void fake_client_destroy(void *user_data)
{
    unsigned *count = user_data;
    ++*count;
}

/* What a foreign teardown can legally do mid-close: count itself, then
 * install a nested server on the same binding. The install-first ordering
 * is what makes the nested close of the just-installed server correct. */
static void reentering_destroy(void *user_data)
{
    ReenteringDestroy *data = user_data;
    ++data->own->destroy_count;
    water_wpe_page_set_asset_server(
        data->page,
        fake_asset_callback,
        data->nested,
        counted_destroy);
}

/* The deterministic ownership check: no message pump, no timing — just the
 * teardown order the fix guarantees against objects the runtime does not
 * own. */
void water_wpe_test_page_callback_lifetime(void)
{
    char *error = NULL;
    WaterWpeRuntime *runtime = water_wpe_runtime_new(&error);
    g_assert(runtime != NULL);

    unsigned client_destroy = 0;
    WaterWpePage *page = water_wpe_page_new(
        runtime,
        fake_event,
        fake_frame,
        fake_message,
        &client_destroy,
        fake_client_destroy,
        &error);
    g_assert(page != NULL);

    /* Emitters that can outlive the page: WebKit's reply task holds the
     * view, and the context and content manager are shared objects. */
    WebKitWebView *web_view = g_object_ref(page->web_view);
    WebKitWebContext *context = g_object_ref(page->web_context);
    WebKitUserContentManager *manager =
        g_object_ref(page->content_manager);

    DestroyCounter first = { 0 };
    DestroyCounter second = { 0 };
    DestroyCounter nested = { 0 };
    ReenteringDestroy reentering = { page, &first, &nested };

    /* The first server's destroy re-enters; the second is the plain one. */
    water_wpe_page_set_asset_server(
        page, fake_asset_callback, &reentering, reentering_destroy);
    WaterWpeAssetBinding *binding = page->asset_binding;
    g_assert(binding != NULL);

    /* Installing the second closes the first: its destroy increments
     * `first` and installs the nested server, which swaps-and-closes the
     * just-installed second — so all three counters are already exact. */
    water_wpe_page_set_asset_server(
        page, fake_asset_callback, &second, counted_destroy);
    g_assert_cmpuint(first.destroy_count, ==, 1);
    g_assert_cmpuint(second.destroy_count, ==, 1);
    g_assert_cmpuint(nested.destroy_count, ==, 0);

    water_wpe_page_free(page);

    g_assert_cmpuint(client_destroy, ==, 1);
    g_assert_cmpuint(nested.destroy_count, ==, 1);

    /* No emitter that outlived the page still carries it as handler data. */
    gpointer emitters[] = { web_view, context, manager };
    for (gsize i = 0; i < G_N_ELEMENTS(emitters); ++i) {
        g_assert(
            g_signal_handler_find(
                emitters[i],
                G_SIGNAL_MATCH_DATA,
                0,
                0,
                NULL,
                NULL,
                page)
            == 0);
    }

    /* The context still owns the binding: closed, not freed. */
    g_assert(binding->callback == NULL);
    g_assert(binding->user_data == NULL);
    g_assert(binding->destroy == NULL);

    /* Releasing the retained objects must not close anything a second
     * time. */
    g_object_unref(web_view);
    g_object_unref(context);
    g_object_unref(manager);
    g_assert_cmpuint(nested.destroy_count, ==, 1);

    water_wpe_runtime_free(runtime);
}
