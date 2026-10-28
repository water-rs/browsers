//! # Safety
//!
//! Resolving a symbol asserts that the bridge library exports it at the declared
//! signature — the contract between this crate and the bridge it is built
//! against. The `Library` outlives every pointer taken out of it.

use std::ffi::{c_char, c_double, c_int, c_uint, c_void};

pub const ABI_VERSION: u32 = 6;
pub const MAX_PLANES: usize = 4;
/// `WATER_WPE_FRAME_DMA_BUF`: the frame carries exported DMA-BUF planes.
pub const FRAME_KIND_DMA_BUF: c_uint = 0;
/// `WATER_WPE_FRAME_SHM`: the frame carries a shared-memory mapping.
pub const FRAME_KIND_SHM: c_uint = 1;

#[repr(C)]
pub struct WaterWpeRuntime {
    _private: [u8; 0],
}

#[repr(C)]
pub struct WaterWpePage {
    _private: [u8; 0],
}

#[repr(C)]
pub struct WaterWpeReply {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WaterWpeBytes {
    pub data: *const u8,
    pub len: usize,
    pub user_data: *mut c_void,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// One frame of page output, tagged by [`WaterWpeFrame::kind`].
///
/// Field order mirrors `WaterWpeFrame` in `native/waterui_wpe.h` exactly, and
/// the tests below pin the offsets it produces: the fields are grouped by width
/// so that neither compiler inserts padding. A field belonging to the other kind
/// is zero, or `-1` for a descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WaterWpeFrame {
    pub token: *mut c_void,
    /// Shared memory: first byte of the top row, valid until the frame's token
    /// is released.
    pub data: *const u8,
    /// Shared memory: bytes `data` addresses.
    pub len: usize,
    /// DMA-BUF: DRM format modifier.
    pub modifier: u64,
    /// [`FRAME_KIND_DMA_BUF`] or [`FRAME_KIND_SHM`].
    pub kind: c_uint,
    pub width: c_uint,
    pub height: c_uint,
    /// DMA-BUF: DRM fourcc.
    pub format: c_uint,
    /// Shared memory: `WPEPixelFormat`.
    pub pixel_format: c_uint,
    /// Shared memory: bytes between adjacent rows.
    pub stride: c_uint,
    /// DMA-BUF: planes described by the three arrays below.
    pub n_planes: c_uint,
    /// DMA-BUF: the buffer's rendering fence, or `-1` when it has none.
    pub rendering_fence_fd: c_int,
    pub fds: [c_int; MAX_PLANES],
    pub offsets: [c_uint; MAX_PLANES],
    pub strides: [c_uint; MAX_PLANES],
}

/// One descriptor the runtime's main context wants watched, in `poll(2)` terms.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WaterWpePollFd {
    pub fd: c_int,
    pub events: i16,
}

/// When the runtime's main context next has work to do.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WaterWpeReadiness {
    pub ready: bool,
    pub timeout_ms: i32,
}

pub type DestroyNotify = unsafe extern "C" fn(*mut c_void);
pub type EventCallback =
    unsafe extern "C" fn(*mut c_void, c_uint, *const c_char, *const c_char, c_double);
pub type FrameCallback = unsafe extern "C" fn(*mut c_void, *const WaterWpeFrame);
/// Receives the calling document's authenticated origin, bridge envelope, and
/// owned reply token.
pub type MessageCallback =
    unsafe extern "C" fn(*mut c_void, *mut WaterWpeReply, *const c_char, *const c_char);
pub type ResultCallback = unsafe extern "C" fn(*mut c_void, bool, *const c_char, usize);

/// One answer to a `waterui://localhost` request. `headers` is `"Name: value"`
/// lines joined by `\n`; each [`WaterWpeBytes`] releases its own storage
/// through `destroy`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WaterWpeAssetResponse {
    pub status: c_uint,
    pub headers: WaterWpeBytes,
    pub body: WaterWpeBytes,
}

/// Answers one request on the page's asset origin from the engine's own
/// method and URI.
pub type AssetCallback =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> WaterWpeAssetResponse;

/// Answers one page-initiated navigation decision synchronously: target URL,
/// the document it leaves (null before the first commit), the main-frame flag,
/// and the user-gesture flag. `false` keeps the engine from proceeding.
pub type PolicyCallback =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, bool, bool) -> bool;

pub struct WpeApi {
    pub runtime_new: unsafe extern "C" fn(*mut *mut c_char) -> *mut WaterWpeRuntime,
    pub runtime_free: unsafe extern "C" fn(*mut WaterWpeRuntime),
    pub runtime_iteration: unsafe extern "C" fn(*mut WaterWpeRuntime) -> bool,
    pub runtime_readiness: unsafe extern "C" fn(
        *mut WaterWpeRuntime,
        *mut WaterWpeReadiness,
        *mut WaterWpePollFd,
        c_uint,
    ) -> c_uint,
    pub string_free: unsafe extern "C" fn(*mut c_char),
    pub page_new: unsafe extern "C" fn(
        *mut WaterWpeRuntime,
        EventCallback,
        FrameCallback,
        MessageCallback,
        *mut c_void,
        DestroyNotify,
        *mut *mut c_char,
    ) -> *mut WaterWpePage,
    pub page_free: unsafe extern "C" fn(*mut WaterWpePage),
    pub page_set_asset_server:
        unsafe extern "C" fn(*mut WaterWpePage, AssetCallback, *mut c_void, DestroyNotify),
    pub page_set_navigation_policy: unsafe extern "C" fn(
        *mut WaterWpePage,
        Option<PolicyCallback>,
        *mut c_void,
        Option<DestroyNotify>,
    ),
    pub page_set_bridge_origins: unsafe extern "C" fn(*mut WaterWpePage, *const c_char),
    pub page_get_web_process_identifier:
        unsafe extern "C" fn(*mut WaterWpePage, ResultCallback, *mut c_void),
    pub reply_return: unsafe extern "C" fn(*mut WaterWpeReply, *const c_char, usize),
    pub reply_free: unsafe extern "C" fn(*mut WaterWpeReply),
    pub page_load_uri: unsafe extern "C" fn(*mut WaterWpePage, *const c_char),
    pub page_go_back: unsafe extern "C" fn(*mut WaterWpePage),
    pub page_go_forward: unsafe extern "C" fn(*mut WaterWpePage),
    pub page_stop: unsafe extern "C" fn(*mut WaterWpePage),
    pub page_reload: unsafe extern "C" fn(*mut WaterWpePage),
    pub page_can_go_back: unsafe extern "C" fn(*mut WaterWpePage) -> bool,
    pub page_can_go_forward: unsafe extern "C" fn(*mut WaterWpePage) -> bool,
    pub page_set_redirects_enabled: unsafe extern "C" fn(*mut WaterWpePage, bool),
    pub page_set_user_agent: unsafe extern "C" fn(*mut WaterWpePage, *const c_char),
    pub page_resize: unsafe extern "C" fn(*mut WaterWpePage, c_uint, c_uint, c_double),
    pub page_set_focus: unsafe extern "C" fn(*mut WaterWpePage, bool),
    pub page_pointer_button:
        unsafe extern "C" fn(*mut WaterWpePage, bool, c_uint, c_double, c_double, c_uint, c_uint),
    pub page_pointer_move: unsafe extern "C" fn(
        *mut WaterWpePage,
        c_double,
        c_double,
        c_double,
        c_double,
        c_uint,
        c_uint,
    ),
    pub page_scroll: unsafe extern "C" fn(
        *mut WaterWpePage,
        c_double,
        c_double,
        c_double,
        c_double,
        bool,
        bool,
        c_uint,
        c_uint,
    ),
    pub page_key: unsafe extern "C" fn(*mut WaterWpePage, bool, c_uint, c_uint, c_uint, c_uint),
    pub page_add_script:
        unsafe extern "C" fn(*mut WaterWpePage, *const c_char, *const c_char, c_uint),
    pub page_set_cookie: unsafe extern "C" fn(*mut WaterWpePage, *const c_char),
    pub page_get_cookies: unsafe extern "C" fn(*mut WaterWpePage, ResultCallback, *mut c_void),
    pub page_run_javascript:
        unsafe extern "C" fn(*mut WaterWpePage, *const c_char, ResultCallback, *mut c_void),
    pub page_call_async_javascript:
        unsafe extern "C" fn(*mut WaterWpePage, *const c_char, ResultCallback, *mut c_void),
    pub frame_presented: unsafe extern "C" fn(*mut c_void),
    pub frame_release: unsafe extern "C" fn(*mut c_void, c_int),
}

/// Resolves one bridge export into its declared type.
///
/// # Safety
/// The caller declares the signature the bridge wrote the symbol with; getting
/// it wrong is the ABI violation `WpeApi::load` is entirely about.
pub unsafe fn symbol<T: Copy>(library: &libloading::Library, name: &[u8]) -> T {
    // SAFETY: symbol resolved from the bridge library, at the signature
    // declared here; see the module safety note.
    unsafe {
        *library
            .get::<T>(name)
            .unwrap_or_else(|error| panic!("bundled WPE ABI symbol is missing: {error}"))
    }
}

impl WpeApi {
    pub unsafe fn load(library: &libloading::Library) -> Self {
        // SAFETY: symbol resolved from the bridge library, at the signature declared
        // here; see the module safety note.
        unsafe {
            Self {
                runtime_new: symbol(library, b"water_wpe_runtime_new\0"),
                runtime_free: symbol(library, b"water_wpe_runtime_free\0"),
                runtime_iteration: symbol(library, b"water_wpe_runtime_iteration\0"),
                runtime_readiness: symbol(library, b"water_wpe_runtime_readiness\0"),
                string_free: symbol(library, b"water_wpe_string_free\0"),
                page_new: symbol(library, b"water_wpe_page_new\0"),
                page_free: symbol(library, b"water_wpe_page_free\0"),
                page_set_asset_server: symbol(library, b"water_wpe_page_set_asset_server\0"),
                page_set_navigation_policy: symbol(
                    library,
                    b"water_wpe_page_set_navigation_policy\0",
                ),
                page_set_bridge_origins: symbol(library, b"water_wpe_page_set_bridge_origins\0"),
                page_get_web_process_identifier: symbol(
                    library,
                    b"water_wpe_page_get_web_process_identifier\0",
                ),
                reply_return: symbol(library, b"water_wpe_reply_return\0"),
                reply_free: symbol(library, b"water_wpe_reply_free\0"),
                page_load_uri: symbol(library, b"water_wpe_page_load_uri\0"),
                page_go_back: symbol(library, b"water_wpe_page_go_back\0"),
                page_go_forward: symbol(library, b"water_wpe_page_go_forward\0"),
                page_stop: symbol(library, b"water_wpe_page_stop\0"),
                page_reload: symbol(library, b"water_wpe_page_reload\0"),
                page_can_go_back: symbol(library, b"water_wpe_page_can_go_back\0"),
                page_can_go_forward: symbol(library, b"water_wpe_page_can_go_forward\0"),
                page_set_redirects_enabled: symbol(
                    library,
                    b"water_wpe_page_set_redirects_enabled\0",
                ),
                page_set_user_agent: symbol(library, b"water_wpe_page_set_user_agent\0"),
                page_resize: symbol(library, b"water_wpe_page_resize\0"),
                page_set_focus: symbol(library, b"water_wpe_page_set_focus\0"),
                page_pointer_button: symbol(library, b"water_wpe_page_pointer_button\0"),
                page_pointer_move: symbol(library, b"water_wpe_page_pointer_move\0"),
                page_scroll: symbol(library, b"water_wpe_page_scroll\0"),
                page_key: symbol(library, b"water_wpe_page_key\0"),
                page_add_script: symbol(library, b"water_wpe_page_add_script\0"),
                page_set_cookie: symbol(library, b"water_wpe_page_set_cookie\0"),
                page_get_cookies: symbol(library, b"water_wpe_page_get_cookies\0"),
                page_run_javascript: symbol(library, b"water_wpe_page_run_javascript\0"),
                page_call_async_javascript: symbol(
                    library,
                    b"water_wpe_page_call_async_javascript\0",
                ),
                frame_presented: symbol(library, b"water_wpe_frame_presented\0"),
                frame_release: symbol(library, b"water_wpe_frame_release\0"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ABI_VERSION, WaterWpeAssetResponse, WaterWpeBytes, WaterWpeFrame, WaterWpeReply};
    use std::ffi::{c_int, c_uint, c_void};
    use std::mem::{align_of, offset_of, size_of};

    /// The bridge writes this struct and this crate reads it, so the two
    /// declarations agreeing is the whole contract. Nothing here can ask the C
    /// compiler what it produced, so what is pinned is the property the C
    /// declaration is written for: the fields are grouped widest-first, in one
    /// order, with no padding anywhere.
    #[test]
    fn frame_layout_is_packed_in_declaration_order() {
        let fields = [
            (
                "token",
                offset_of!(WaterWpeFrame, token),
                size_of::<*mut c_void>(),
            ),
            (
                "data",
                offset_of!(WaterWpeFrame, data),
                size_of::<*const u8>(),
            ),
            ("len", offset_of!(WaterWpeFrame, len), size_of::<usize>()),
            (
                "modifier",
                offset_of!(WaterWpeFrame, modifier),
                size_of::<u64>(),
            ),
            ("kind", offset_of!(WaterWpeFrame, kind), size_of::<c_uint>()),
            (
                "width",
                offset_of!(WaterWpeFrame, width),
                size_of::<c_uint>(),
            ),
            (
                "height",
                offset_of!(WaterWpeFrame, height),
                size_of::<c_uint>(),
            ),
            (
                "format",
                offset_of!(WaterWpeFrame, format),
                size_of::<c_uint>(),
            ),
            (
                "pixel_format",
                offset_of!(WaterWpeFrame, pixel_format),
                size_of::<c_uint>(),
            ),
            (
                "stride",
                offset_of!(WaterWpeFrame, stride),
                size_of::<c_uint>(),
            ),
            (
                "n_planes",
                offset_of!(WaterWpeFrame, n_planes),
                size_of::<c_uint>(),
            ),
            (
                "rendering_fence_fd",
                offset_of!(WaterWpeFrame, rendering_fence_fd),
                size_of::<c_int>(),
            ),
            (
                "fds",
                offset_of!(WaterWpeFrame, fds),
                size_of::<[c_int; super::MAX_PLANES]>(),
            ),
            (
                "offsets",
                offset_of!(WaterWpeFrame, offsets),
                size_of::<[c_uint; super::MAX_PLANES]>(),
            ),
            (
                "strides",
                offset_of!(WaterWpeFrame, strides),
                size_of::<[c_uint; super::MAX_PLANES]>(),
            ),
        ];
        let mut next = 0;
        for (name, offset, size) in fields {
            assert_eq!(
                offset, next,
                "WaterWpeFrame.{name} is not where the C declaration puts it"
            );
            next = offset + size;
        }
        assert_eq!(
            size_of::<WaterWpeFrame>(),
            next,
            "WaterWpeFrame must carry no tail padding"
        );
        assert_eq!(align_of::<WaterWpeFrame>(), align_of::<u64>());
    }

    /// `WaterWpeBytes` crosses in both directions — script replies out, asset
    /// headers and bodies in — and the bridge frees them through `destroy` on
    /// `user_data`, so its layout is pinned the same way the frame's is.
    #[test]
    fn bytes_layout_matches_the_bridge_declaration() {
        assert_eq!(offset_of!(WaterWpeBytes, data), 0);
        assert_eq!(offset_of!(WaterWpeBytes, len), size_of::<*const u8>());
        assert_eq!(
            offset_of!(WaterWpeBytes, user_data),
            size_of::<*const u8>() + size_of::<usize>()
        );
        assert_eq!(
            offset_of!(WaterWpeBytes, destroy),
            size_of::<*const u8>() + size_of::<usize>() + size_of::<*mut c_void>()
        );
    }

    /// The reply token is opaque on both sides: the bridge allocates and owns
    /// it, and this crate only ever passes the pointer back, which is what the
    /// empty private array guarantees.
    #[test]
    fn reply_token_is_opaque() {
        assert_eq!(size_of::<WaterWpeReply>(), 0);
    }

    /// `WaterWpeAssetResponse` hands the bridge one status plus two byte
    /// spans; the fields are pinned so a future layout change fails loudly.
    #[test]
    fn asset_response_layout_matches_the_bridge_declaration() {
        assert_eq!(
            offset_of!(WaterWpeAssetResponse, headers),
            size_of::<c_uint>().next_multiple_of(align_of::<WaterWpeBytes>())
        );
        assert_eq!(
            offset_of!(WaterWpeAssetResponse, body),
            offset_of!(WaterWpeAssetResponse, headers) + size_of::<WaterWpeBytes>()
        );
    }

    /// The loader asserts `abi_version() == ABI_VERSION` before it resolves any
    /// other symbol, so pinning the constant pins which bridge generations are
    /// accepted: ABI 6 added the navigation-policy callback, and a bridge still
    /// answering 5 is rejected at load.
    #[test]
    fn the_expected_abi_version_is_six() {
        assert_eq!(ABI_VERSION, 6);
    }
}
