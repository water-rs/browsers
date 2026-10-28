//! What only a real engine can prove.
//!
//! These tests drive a genuine WPE `WebKit` runtime through the same handle
//! `WpeController::open` hands the renderer, load pages over a local HTTP
//! server, and assert on what crosses the bridge in both directions.
//!
//! They exist because three total breaks of the web view bridge shipped behind
//! a fully green Rust suite: every reply crossing as base64, a frozen `waterui`
//! object that made `waterui.state` and `waterui.watch` throw on install, and
//! integers past 2^53 losing their low bits in both directions. None of the
//! three is visible to a test that stops at the Rust side of the boundary.
//!
//! # Running them
//!
//! They are behind the `real-engine` feature, so an ordinary `cargo nextest run`
//! neither builds nor reports them — a test that cannot run must not look like
//! one that passed. With the feature on, a staged runtime is mandatory and its
//! absence is a loud failure, never a skip:
//!
//! ```sh
//! browser-wpe/runtime/build-runtime.sh wpe-artifacts
//! unzip -q wpe-artifacts/waterui-wpe-*.zip -d wpe-runtime
//! WATERUI_WPE_RUNTIME="$PWD/wpe-runtime" \
//!     cargo nextest run -p waterui-browser-wpe --features real-engine
//! ```
//!
//! `build-runtime.sh` compiles WPE `WebKit` from the pinned source tarball,
//! which takes hours, and it discards its own build tree — the packaged archive
//! is what it leaves behind. Unpacking that archive is also what `water package`
//! does, so `WATERUI_WPE_RUNTIME` names exactly the layout
//! [`WpeRuntimePaths::packaged`] resolves next to a shipped executable.
//!
//! # Why there is no libtest harness
//!
//! `WpeRuntime` is a main-thread runtime: the thread that creates it is
//! `WebKit`'s UI thread, and `WebKit` asserts as much when it tears down. libtest
//! runs every `#[test]` body on a thread it spawns, so a test could never
//! honour that contract; the process's `exit()` then finalizes `WebKit`'s
//! network session on the main thread while the runtime was made on another,
//! and the engine aborts. The target is `harness = false` and `main` below
//! runs the checks with `libtest-mimic`, pinned to one test thread, which it
//! runs inline on the calling thread: the process main thread. Each check
//! still gets a process of its own from `cargo nextest run`, which lists the
//! checks and runs one per process, so a runtime is never created twice in
//! one process.
#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use executor_core::LocalExecutor;
use executor_core::async_executor::AsyncLocalExecutor;
use libtest_mimic::{Arguments, Trial};
use serde_json::Value;
use tiny_http::{Header, Response, Server};
use waterui_browser_wpe::{WpePage, WpeRuntime, WpeRuntimePaths, WpeWebViewHandle};
use waterui_url::Url;
use waterui_webview::conformance::raw_evaluation_answers_json;
use waterui_webview::{
    BackendEvent, BridgeOrigins, IntoJsReply, JsReply, Json, NavigationPolicy, OriginPolicy,
    ScriptInjectionTime, ScriptMessageHandler, UserScript, WatcherGuard, WebViewConfig,
    WebViewEvent, WebViewHandle as _, stay_on_origin,
};

/// Names the staged WPE runtime root these tests drive.
const RUNTIME_ENV: &str = "WATERUI_WPE_RUNTIME";

/// How long one wait may take before the test gives up.
///
/// Generous on purpose: a cold WPE runtime spawns a web process and a network
/// process before the first byte of the page is parsed, and CI runners are
/// slower than they look.
const TIMEOUT: Duration = Duration::from_mins(1);

/// `2^53 + 1`: the smallest integer a JavaScript number cannot hold.
const UNREPRESENTABLE: u64 = 9_007_199_254_740_993;

/// An integer a JavaScript number holds exactly, which must stay an ordinary
/// number rather than being tagged along with the one above.
const REPRESENTABLE: u64 = 42;

// The pages live at the workspace root because they exercise the shared bridge
// contract; every real-engine suite in this repository loads the same ones.
const FIRST_HTML: &str = include_str!("../../tests/pages/first.html");
const SECOND_HTML: &str = include_str!("../../tests/pages/second.html");
const CHECKS_JS: &str = include_str!("../../tests/pages/checks.js");
const STATE_SEED_JS: &str = include_str!("../../tests/pages/state_seed.js");

// This one is not shared: it is about WPE's own main loop rather than about the
// bridge contract, and it is injected by the one test that drives the pump.
const IDLE_TIMER_JS: &str = include_str!("pages/idle_timer.js");

/// The local executor the page's handler replies are spawned onto.
///
/// `WpePage` answers a `waterui.invoke` by spawning the handler's future with
/// `executor_core::spawn_local` and evaluating the reply script when it
/// resolves, so a test that never drives a local executor would leave every
/// call pending forever. The engine's own loop is pumped in the same step, so
/// the executor has to be one this test can tick rather than one that blocks.
#[derive(Clone, Debug)]
struct TestExecutor(Rc<AsyncLocalExecutor<'static>>);

impl LocalExecutor for TestExecutor {
    type Task<T: 'static> = <AsyncLocalExecutor<'static> as LocalExecutor>::Task<T>;

    fn spawn_local<Fut>(&self, future: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        self.0.spawn_local(future)
    }
}

/// Raises `GLib` `g_critical` to a trap for the life of one test, restored
/// when it drops. The library mapping the symbol lives in the scope, so the
/// stored pointer outlives every call into it.
struct GlibCriticalScope {
    _library: libloading::Library,
    set: unsafe extern "C" fn(u32) -> u32,
    previous: u32,
}

impl GlibCriticalScope {
    fn new(paths: &WpeRuntimePaths) -> Self {
        const G_LOG_LEVEL_CRITICAL: u32 = 1 << 3;
        // SAFETY: the library is the runtime's own glib; the symbol is the
        // documented `g_log_set_always_fatal` C ABI, and `_library` keeps the
        // mapping alive past every call.
        let library = unsafe {
            libloading::Library::new(paths.root().join("lib/libglib-2.0.so.0"))
                .expect("the staged runtime carries libglib-2.0")
        };
        // SAFETY: `g_log_set_always_fatal` has the C signature (GLogLevelFlags
        // -> GLogLevelFlags), both of which are u32 on this ABI.
        let set = unsafe {
            *library
                .get::<unsafe extern "C" fn(u32) -> u32>(b"g_log_set_always_fatal\0")
                .expect("glib exports g_log_set_always_fatal")
        };
        // SAFETY: `set` points into the retained library and takes the mask
        // by value, exactly as the C prototype declares. The first call reads
        // the previous mask out; the second installs the OR'd set so fatal
        // flags a host already carried stay set.
        let previous = unsafe {
            let previous = set(G_LOG_LEVEL_CRITICAL);
            set(previous | G_LOG_LEVEL_CRITICAL);
            previous
        };
        Self {
            _library: library,
            set,
            previous,
        }
    }
}

impl Drop for GlibCriticalScope {
    fn drop(&mut self) {
        // SAFETY: `Drop` runs before the `_library` field is released, so
        // the pointer is still valid for the restore.
        unsafe { (self.set)(self.previous) };
    }
}

/// One WPE runtime, one page, and the server the page is loaded from.
struct RealEngine {
    server: Server,
    base: String,
    runtime: WpeRuntime,
    handle: WpeWebViewHandle,
    executor: TestExecutor,
    events: Rc<RefCell<Vec<BackendEvent>>>,
    reports: Rc<RefCell<Vec<Value>>>,
    echoed: Rc<RefCell<Option<Value>>>,
    /// Dropping this unsubscribes, and every wait below is on an event.
    _events: WatcherGuard,
    /// Declared last: it restores the fatal mask only after every engine
    /// field above has dropped.
    _criticals: GlibCriticalScope,
}

impl core::fmt::Debug for RealEngine {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RealEngine")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Wraps a synchronous answer in the asynchronous shape every handler has.
fn handler(
    answer: impl Fn(&[u8]) -> Result<JsReply, String> + 'static,
) -> Box<ScriptMessageHandler> {
    Box::new(move |payload: &[u8]| {
        let reply = answer(payload);
        Box::pin(async move { reply })
    })
}

/// The staged runtime, or a failure that says how to stage one.
fn runtime_paths() -> WpeRuntimePaths {
    let root = std::env::var_os(RUNTIME_ENV).unwrap_or_else(|| {
        panic!(
            "{RUNTIME_ENV} is not set. These tests drive a real WPE WebKit runtime rather than a \
             stand-in: build one with browser-wpe/runtime/build-runtime.sh and \
             point {RUNTIME_ENV} at the staged root holding lib/libwaterui_wpe.so, runtime.json \
             and licenses/."
        )
    });
    WpeRuntimePaths::new(root)
}

impl RealEngine {
    /// Starts the server, initializes the runtime and opens one page.
    fn start() -> Self {
        Self::start_with(None)
    }

    /// Starts the same engine with `policy` installed on the view's config.
    fn start_with(navigation_policy: Option<NavigationPolicy>) -> Self {
        // Criticals go fatal before any runtime work starts, and the scope
        // restores the mask only after every engine field has dropped.
        let paths = runtime_paths();
        let criticals = GlibCriticalScope::new(&paths);
        let server = Server::http("127.0.0.1:0").expect("bind the local page server");
        let address = server
            .server_addr()
            .to_ip()
            .expect("the local page server listens on a TCP port");
        let base = format!("http://{address}");

        let executor = TestExecutor(Rc::new(AsyncLocalExecutor::new()));
        executor_core::init_local_executor(executor.clone());

        // The runtime is kept so one test can start its message pump; every
        // other test drives the loop by hand from `step`.
        let runtime = WpeRuntime::initialize(&paths);
        let page = WpePage::new(runtime.clone());
        // A viewport the engine would lay a document out in; the default is the
        // one-pixel toplevel the bridge creates a view with.
        page.resize(1024, 768, 1.0);
        // Exactly what `WpeController::open` builds: the handle installs the
        // transport adapter and the shared document-start bridge script.
        // And what the framework does next, which a handle built by hand does
        // not get: every registered handler is a capability, so the backend
        // denies a document it cannot authenticate — and with no policy at all
        // that is every document, silently, from the page's point of view. An
        // application opened at this URL is granted its own origin, which is
        // what `BridgeOrigins::Initial` means — handed in the same way
        // `WpeController::open` hands it, through the view's config.
        let mut config = WebViewConfig::default();
        config.scripts = vec![UserScript::new(
            "waterui:test-state-seed",
            STATE_SEED_JS,
            ScriptInjectionTime::DocumentStart,
        )];
        config.bridge_origins = OriginPolicy::new(
            BridgeOrigins::Initial,
            &base
                .parse()
                .expect("the local page server address is a URL"),
        );
        config.navigation_policy = navigation_policy;
        let handle = WpeWebViewHandle::new(page, config);

        let events = Rc::new(RefCell::new(Vec::new()));
        let guard = handle.watch({
            let events = Rc::clone(&events);
            move |event| events.borrow_mut().push(event)
        });

        let reports = Rc::new(RefCell::new(Vec::new()));
        let echoed = Rc::new(RefCell::new(None));

        handle.add_handler(
            "greet",
            handler(|payload| {
                let request: Value =
                    serde_json::from_slice(payload).map_err(|error| error.to_string())?;
                let name = request
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| String::from("greet payload has no `name` string"))?;
                format!("Hi {name}").into_js_reply()
            }),
        );
        handle.add_handler(
            "largest-id",
            handler(|_| Json(UNREPRESENTABLE).into_js_reply()),
        );
        handle.add_handler("small-id", handler(|_| Json(REPRESENTABLE).into_js_reply()));
        handle.add_handler("echo-id", {
            let echoed = Rc::clone(&echoed);
            handler(move |payload| {
                let request: Value =
                    serde_json::from_slice(payload).map_err(|error| error.to_string())?;
                echoed.replace(Some(request));
                String::from_utf8(payload.to_vec())
                    .map_err(|error| error.to_string())?
                    .into_js_reply()
            })
        });
        handle.add_handler("report", {
            let reports = Rc::clone(&reports);
            handler(move |payload| {
                let record: Value =
                    serde_json::from_slice(payload).map_err(|error| error.to_string())?;
                reports.borrow_mut().push(record);
                ().into_js_reply()
            })
        });

        Self {
            server,
            base,
            runtime,
            handle,
            executor,
            events,
            reports,
            echoed,
            _events: guard,
            _criticals: criticals,
        }
    }

    /// Answers whatever the engine has asked for, without blocking on it.
    ///
    /// Driven from the same loop that pumps the engine, so the page is served
    /// by this thread rather than by a second one racing it.
    fn serve(&self) {
        while let Some(request) = self
            .server
            .try_recv()
            .expect("the local page server stopped accepting requests")
        {
            let served = match request.url() {
                "/first" => Some((FIRST_HTML, "text/html; charset=utf-8")),
                "/second" => Some((SECOND_HTML, "text/html; charset=utf-8")),
                "/checks.js" => Some((CHECKS_JS, "text/javascript; charset=utf-8")),
                // Engines ask for things nobody linked, a favicon above all.
                // Saying so is the correct answer, not a test failure.
                _ => None,
            };
            let response = match served {
                Some((body, content_type)) => {
                    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                        .expect("a static content type is a valid header");
                    Response::from_string(body).with_header(header)
                }
                None => Response::from_string(String::new()).with_status_code(404),
            };
            request
                .respond(response)
                .expect("the engine closed the connection before its page arrived");
        }
    }

    /// One turn of everything this test drives.
    fn step(&self) {
        self.serve();
        self.handle.page().pump();
        while self.executor.0.try_tick() {}
    }

    /// One turn of everything except the engine's own loop.
    ///
    /// The engine is deliberately left alone here: whatever WPE gets done under
    /// this step is done by its message pump, which is a task on the very
    /// executor being ticked.
    fn idle_step(&self) {
        self.serve();
        while self.executor.0.try_tick() {}
    }

    /// Spins until `ready` answers, failing loudly on a load error or a timeout.
    fn wait_for<T>(&self, what: &str, ready: impl FnMut() -> Option<T>) -> T {
        self.wait_with(Self::step, what, ready)
    }

    /// [`Self::wait_for`] without pumping the engine by hand.
    fn wait_idle<T>(&self, what: &str, ready: impl FnMut() -> Option<T>) -> T {
        self.wait_with(Self::idle_step, what, ready)
    }

    fn wait_with<T>(&self, step: fn(&Self), what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(value) = ready() {
                return value;
            }
            self.assert_no_load_error();
            assert!(
                Instant::now() < deadline,
                "timed out after {TIMEOUT:?} waiting for {what}"
            );
            step(self);
            std::thread::yield_now();
        }
    }

    /// Drives one of the handle's futures to completion on this loop.
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        self.wait_for("an engine call to answer", || {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => Some(value),
                Poll::Pending => None,
            }
        })
    }

    fn assert_no_load_error(&self) {
        for event in self.events.borrow().iter() {
            assert!(
                !matches!(event, BackendEvent::Event(WebViewEvent::Error(_))),
                "the engine reported a load failure: {event:?}"
            );
        }
    }

    fn url(&self, path: &str) -> Url {
        format!("{}{path}", self.base)
            .parse()
            .expect("the local page server address is a URL")
    }

    fn loads(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, BackendEvent::Event(WebViewEvent::Loaded)))
            .count()
    }

    /// Navigates and waits for the page to finish loading.
    fn navigate(&self, what: &str, act: impl FnOnce()) {
        let loads = self.loads();
        act();
        self.wait_for(what, || (self.loads() > loads).then_some(()));
    }

    /// Loads `path` and returns the record its script sent back.
    fn open(&self, path: &str) -> Value {
        let reported = self.reports.borrow().len();
        self.navigate(path, || self.handle.go_to(&self.url(path)));
        let record = self.wait_for(&format!("{path} to report over the bridge"), || {
            self.reports.borrow().get(reported).cloned()
        });
        if let Some(failure) = record.get("failure") {
            panic!("the page's checks raised {failure}");
        }
        record
    }

    /// What the engine says the current document's URL is.
    fn location(&self) -> String {
        let json = self
            .block_on(self.handle.run_javascript("location.href"))
            .expect("location.href evaluates");
        serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("run_javascript answered `{json}`, not JSON: {error}"))
    }
}

/// Reads one field of the page's record, saying what was there when it is
/// missing or the wrong shape.
fn field<'a>(record: &'a Value, name: &str) -> &'a Value {
    record
        .get(name)
        .unwrap_or_else(|| panic!("the page recorded no `{name}`; it recorded {record}"))
}

fn text<'a>(record: &'a Value, name: &str) -> &'a str {
    field(record, name)
        .as_str()
        .unwrap_or_else(|| panic!("the page's `{name}` is not text; it recorded {record}"))
}

fn navigation_reaches_each_url_and_history_moves_both_ways() {
    let engine = RealEngine::start();

    let first = engine.open("/first");
    assert_eq!(text(&first, "page"), "first");
    assert_eq!(text(&first, "location"), engine.url("/first").as_str());
    assert_eq!(engine.location(), engine.url("/first").as_str());
    assert!(
        !engine.handle.can_go_back(),
        "the first document in a page's history has nothing behind it"
    );

    let second = engine.open("/second");
    assert_eq!(text(&second, "page"), "second");
    assert_eq!(engine.location(), engine.url("/second").as_str());
    assert!(engine.handle.can_go_back());
    assert!(!engine.handle.can_go_forward());

    engine.navigate("the engine to go back", || engine.handle.go_back());
    assert_eq!(engine.location(), engine.url("/first").as_str());
    assert!(engine.handle.can_go_forward());

    engine.navigate("the engine to go forward", || engine.handle.go_forward());
    assert_eq!(engine.location(), engine.url("/second").as_str());
    assert!(!engine.handle.can_go_forward());
}

/// The raw evaluation path answers with the JSON encoding of the value.
///
/// Unquoted, an answer is not parseable: `location.href` and a page that
/// returned the six characters `"first"` are the same nine bytes, and the
/// fixture's own `location` reader has to guess which it got. The engine's
/// `jsc_value_to_json` is what encodes it, so a string arrives quoted and an
/// object arrives as an object.
fn the_raw_evaluation_path_answers_with_json() {
    let engine = RealEngine::start();
    engine.open("/first");

    engine.block_on(raw_evaluation_answers_json(async |script| {
        engine.handle.run_javascript(script).await
    }));
}

/// The reply a handler returns has to arrive as the value it returned.
///
/// Every reply once crossed as base64, because the bridge had collapsed a
/// serialized value and a byte payload into the same `Vec<u8>` and could no
/// longer tell them apart. `await waterui.invoke(...)` then resolved to base64
/// text, which is a string of the right type and entirely the wrong value —
/// so the type alone is not the assertion, the value is.
fn a_handler_reply_reaches_the_page_as_its_value_and_not_as_base64() {
    let engine = RealEngine::start();
    let record = engine.open("/first");

    assert_eq!(text(&record, "greetingType"), "string");
    assert_eq!(text(&record, "greeting"), "Hi Lexo");
    assert_ne!(
        text(&record, "greeting"),
        STANDARD.encode(br#""Hi Lexo""#),
        "the reply reached the page as the base64 of its JSON"
    );
}

/// `waterui.state` and `waterui.watch` have to exist in the page.
///
/// `bridge.js` froze the `waterui` object, and `state.js` runs after it and
/// defines both properties on that same object, so installing threw and the
/// entire mirrored-state feature was unreachable from JavaScript for the whole
/// life of the feature. Reading a seeded key back is the part that proves the
/// mirror is wired rather than merely present.
fn the_page_can_reach_the_whole_waterui_object() {
    let engine = RealEngine::start();
    let record = engine.open("/first");

    assert_eq!(text(&record, "invokeType"), "function");
    assert_eq!(text(&record, "stateType"), "object");
    assert_eq!(text(&record, "watchType"), "function");
    assert_eq!(text(&record, "stateTheme"), "dark");
    assert_eq!(
        text(&record, "watchResultType"),
        "function",
        "watching a key answers with the function that stops watching it"
    );
}

/// Integers past 2^53 have to survive the crossing in both directions.
///
/// JSON has one numeric type and JavaScript reads it as a double, so these lost
/// their low bits silently and symmetrically: a value seeded into a page read
/// back rounded, and a page that returned the value it was given returned the
/// rounded one. Both ends looked correct the whole time.
fn integers_beyond_two_to_the_fifty_third_cross_intact_both_ways() {
    let engine = RealEngine::start();
    let record = engine.open("/first");

    // Rust to the page, through a handler reply and through mirrored state.
    assert_eq!(text(&record, "largeType"), "bigint");
    assert_eq!(text(&record, "largeText"), UNREPRESENTABLE.to_string());
    assert_eq!(text(&record, "stateBigType"), "bigint");
    assert_eq!(text(&record, "stateBigText"), UNREPRESENTABLE.to_string());

    // A value a double does hold stays an ordinary number, so page arithmetic
    // on everyday values keeps working.
    assert_eq!(text(&record, "smallType"), "number");
    assert_eq!(field(&record, "smallValue").as_u64(), Some(REPRESENTABLE));

    // The page back to Rust.
    let echoed = engine
        .echoed
        .borrow()
        .clone()
        .expect("the page called the echo handler");
    assert_eq!(
        echoed
            .pointer("/id/__wateruiBigInt")
            .and_then(Value::as_str),
        Some(UNREPRESENTABLE.to_string().as_str()),
        "the page's BigInt reached Rust as {echoed}"
    );
    assert_eq!(
        echoed.pointer("/small").and_then(Value::as_u64),
        Some(REPRESENTABLE)
    );
}

/// A page that is idle has to keep making progress.
///
/// This is water-rs/browsers#1. WPE's engine work is a `GMainContext`, which
/// runs only while somebody iterates it, and the only thing iterating it was
/// `DmaBufGpuView::render`. A renderer that skips idle frames therefore stopped
/// the engine exactly when a page had nothing new to draw, and a background
/// timer, a network completion or a DOM mutation waited for an unrelated event —
/// a moved pointer — to force a frame that would let it run.
///
/// So this test never touches the engine's loop: it serves the page, ticks the
/// executor, and everything WPE gets done is done by the message pump task.
/// Without that task nothing is iterated at all and the page does not even load.
fn a_background_timer_reaches_the_bridge_without_input_or_rendering() {
    let engine = RealEngine::start();
    engine.handle.inject_script(
        "waterui:test-idle-timer",
        IDLE_TIMER_JS,
        ScriptInjectionTime::DocumentStart,
    );
    engine.runtime.start_message_pump();

    let reported = engine.reports.borrow().len();
    engine.handle.go_to(&engine.url("/first"));
    let record = engine.wait_idle("the page's background timer to report", || {
        engine
            .reports
            .borrow()
            .iter()
            .skip(reported)
            .find(|record| record.get("idleTimer").is_some())
            .cloned()
    });

    assert_eq!(
        text(&record, "idleTimer"),
        "fired",
        "the timer reported without having applied its DOM mutation"
    );
}

/// The page-callback lifetime fixture: builds the whitebox shared object the
/// native `CMake` option produces, loads it against this process's staged
/// runtime, and runs the deterministic ownership assertions inside it.
///
/// No `RealEngine`, no `WpeRuntime` — loading the bridge twice would register
/// its private `GTypes` again, so the fixture runs in this isolated test
/// process instead.
fn page_teardown_disconnects_retained_emitters_and_closes_assets() {
    let paths = runtime_paths();
    let _criticals = GlibCriticalScope::new(&paths);
    let native = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("native");
    let build_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("wpe-lifetime-build-{}", std::process::id()));
    let output_name = format!("wpe_lifetime_test_{}", std::process::id());
    let pkg_config_path = std::env::join_paths(
        [
            paths.root().join("lib/pkgconfig"),
            paths.root().join("share/pkgconfig"),
        ]
        .into_iter()
        .chain(
            std::env::var_os("PKG_CONFIG_PATH")
                .as_deref()
                .map(std::env::split_paths)
                .into_iter()
                .flatten(),
        ),
    )
    .expect("the scoped pkg-config path joins");

    // Both `CMake` invocations share one budget; `timeout` bounds the whole
    // process group so a wedged configure cannot hang the test.
    let deadline = Instant::now() + TIMEOUT;
    let run = |args: &[std::ffi::OsString]| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the lifetime fixture build ran out of budget"
        );
        let seconds = remaining.as_secs().max(1);
        let output = std::process::Command::new("timeout")
            .arg("--kill-after=5s")
            .arg(format!("{seconds}s"))
            .arg("cmake")
            .args(args)
            .env("PKG_CONFIG_PATH", &pkg_config_path)
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) => panic!("cmake could not run: {error}"),
        };
        assert!(
            output.status.success(),
            "cmake failed for the lifetime fixture (status {}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run(&[
        "-S".into(),
        native.as_os_str().to_owned(),
        "-B".into(),
        build_dir.as_os_str().to_owned(),
        "-DCMAKE_BUILD_TYPE=Release".into(),
        "-DWATER_WPE_BUILD_LIFETIME_TEST=ON".into(),
        format!("-DWATER_WPE_TEST_RUNTIME_ROOT={}", paths.root().display()).into(),
        format!("-DWATER_WPE_TEST_OUTPUT_NAME={output_name}").into(),
    ]);
    run(&[
        "--build".into(),
        build_dir.as_os_str().to_owned(),
        "--target".into(),
        "wpe_lifetime_test".into(),
    ]);

    // The fixture is loaded for the rest of the process — its embedded copy
    // of the bridge registers private `GTypes` that keep code pointers, so
    // the library must stay mapped like the production bridge does.
    let library = std::mem::ManuallyDrop::new(unsafe {
        // SAFETY: the fixture's path is inside the staged runtime root this
        // test built it into.
        libloading::Library::new(paths.root().join(format!("lib/lib{output_name}.so")))
            .expect("the lifetime fixture loads")
    });
    unsafe {
        // SAFETY: the fixture's sole export is a `void(void)` entry the C
        // file defines; the library stays mapped for the process's life.
        let entry = library
            .get::<unsafe extern "C" fn()>(b"water_wpe_test_page_callback_lifetime\0")
            .expect("the lifetime fixture exports its entry point");
        entry();
    }
}

/// The navigation policy answers page-initiated navigations while the
/// application's own loads keep working.
///
/// `stay_on_origin` sends a `window.location` jump off the view's origin out —
/// the shared layer opens it externally and answers "cancel" — so the view has
/// to stay put, on a document whose bridge still works. A subframe's own load
/// is a navigation too, and the request it produces has to arrive as one:
/// `is_main_frame` false, and allowed rather than cancelled.
fn the_navigation_policy_keeps_the_view_on_its_origin() {
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = RealEngine::start_with(Some(NavigationPolicy::new({
        let requests = std::sync::Arc::clone(&requests);
        move |request| {
            requests
                .lock()
                .expect("the policy record lock is poisoned")
                .push((request.url.as_str().to_owned(), request.is_main_frame));
            stay_on_origin(request)
        }
    })));
    engine.open("/first");

    // A page-initiated cross-origin jump is denied: the view stays where it
    // was, and the denial must not wedge the page or its bridge — a fresh
    // application-driven load after it both navigates and answers.
    engine
        .block_on(
            engine
                .handle
                .run_javascript("window.location.assign('https://example.com/'); 'assigned'"),
        )
        .expect("the assignment evaluates");
    for _ in 0..200 {
        engine.step();
    }
    assert_eq!(
        engine.location(),
        engine.url("/first").as_str(),
        "a page-initiated cross-origin navigation moved the view"
    );
    engine.open("/second");

    // A subframe's own load reaches the policy as a subframe and is allowed
    // through — `stay_on_origin` admits every non-main-frame navigation.
    engine
        .block_on(engine.handle.run_javascript(
            "document.body.innerHTML = '<iframe src=\"/first\"></iframe>'; 'framed'",
        ))
        .expect("the iframe injects");
    engine.wait_for("the subframe's navigation to reach the policy", || {
        requests
            .lock()
            .expect("the policy record lock is poisoned")
            .iter()
            .any(|(url, is_main_frame)| !is_main_frame && url.ends_with("/first"))
            .then_some(())
    });
    assert!(
        requests
            .lock()
            .expect("the policy record lock is poisoned")
            .iter()
            .any(|(url, is_main_frame)| *is_main_frame && url.contains("example.com")),
        "the cross-origin main-frame request never reached the policy"
    );
}

fn main() {
    let mut args = Arguments::from_args();
    // libtest-mimic runs trials inline on the calling thread only when it is
    // told to use a single thread. That thread is the process main thread,
    // which `WpeRuntime` requires; any other count spawns workers.
    args.test_threads = Some(1);
    // Each trial is named after the function it runs, as the `#[test]` it
    // replaced was, so nextest filters and CI history keep matching.
    macro_rules! trials {
        ($($check:ident),* $(,)?) => {
            vec![$(Trial::test(stringify!($check), || {
                $check();
                Ok(())
            })),*]
        };
    }
    let trials = trials![
        navigation_reaches_each_url_and_history_moves_both_ways,
        the_raw_evaluation_path_answers_with_json,
        a_handler_reply_reaches_the_page_as_its_value_and_not_as_base64,
        the_page_can_reach_the_whole_waterui_object,
        integers_beyond_two_to_the_fifty_third_cross_intact_both_ways,
        a_background_timer_reaches_the_bridge_without_input_or_rendering,
        page_teardown_disconnects_retained_emitters_and_closes_assets,
        the_navigation_policy_keeps_the_view_on_its_origin,
    ];
    libtest_mimic::run(&args, trials).exit();
}
