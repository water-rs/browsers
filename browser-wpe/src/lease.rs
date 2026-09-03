//! WPE's half of the frame contract: the buffer lease.
//!
//! The frames themselves — [`DmaBufFrame`] and [`ShmFrame`] — describe pixels
//! and know nothing about the engine that produced them. What is WPE's is the
//! *lease*: the engine hands out a buffer from its own pool and wants it back
//! once the compositor has read it, which is the two-step present/release
//! protocol [`WpeFrameLease`] implements for both kinds.
//!
//! # Safety
//!
//! As in `page`, the `unsafe` here is calls through the WPE bridge ABI. The
//! function pointers come from a `RuntimeApi` the runtime keeps mapped, and the
//! frame pointer is the one this lease owns. The bridge marshals the WPE object
//! operations back onto the runtime's `GMainContext`.

use std::ffi::c_uint;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use wgpu_external_frame::dma_buf::{DmaBufFormat, DmaBufFrame, DmaBufLease, DmaBufPlane};

use crate::abi::{FRAME_KIND_DMA_BUF, FRAME_KIND_SHM, MAX_PLANES, WaterWpeFrame};
use crate::frame::{BrowserFrame, ShmFormat, ShmFrame};
use crate::runtime::RuntimeApi;

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `WPE_PIXEL_FORMAT_ARGB8888`, the only value `WPEPixelFormat` defines.
const WPE_PIXEL_FORMAT_ARGB8888: c_uint = 0;

/// Reads a `WPEPixelFormat` value.
///
/// # Panics
///
/// Panics for any value WPE Platform's enumeration does not define.
fn shm_format_from_abi(value: c_uint) -> ShmFormat {
    match value {
        WPE_PIXEL_FORMAT_ARGB8888 => ShmFormat::Argb8888,
        _ => panic!(
            "unsupported WPE shared-memory pixel format {value}; \
             the bridge carries WPE_PIXEL_FORMAT_ARGB8888"
        ),
    }
}

/// Builds an owned frame from a WPE bridge frame, taking over its descriptors.
///
/// # Panics
///
/// Panics when the bridge reports a frame this crate cannot describe: a
/// zero-sized one, an unknown buffer kind, or a buffer whose own fields
/// contradict each other.
///
/// # Safety
///
/// `frame` must be a live bridge frame whose descriptors and token this call
/// takes ownership of; see the module safety note.
pub unsafe fn frame_from_abi(api: Arc<RuntimeApi>, frame: &WaterWpeFrame) -> BrowserFrame {
    assert!(
        frame.width > 0 && frame.height > 0,
        "WPE returned a zero-sized frame"
    );
    // Built before the kind is examined, so that every path out of this function
    // — the panic on a kind it does not know included — still returns the buffer
    // to WPE when the lease drops.
    let lease = Box::new(WpeFrameLease {
        api,
        token: frame.token,
        presented: false,
        released: false,
    });
    match frame.kind {
        FRAME_KIND_DMA_BUF => BrowserFrame::DmaBuf(
            // SAFETY: the descriptors this takes over belong to the frame the
            // lease now owns; see the module safety note.
            unsafe { dma_buf_from_abi(frame, lease) },
        ),
        FRAME_KIND_SHM => BrowserFrame::Shm(
            // SAFETY: the bridge keeps the buffer and its bytes referenced by
            // the token this lease holds, so the mapping stays valid — and, as
            // WPE does not draw into a buffer it has leased out, unchanged —
            // until the lease is released.
            unsafe { shm_from_abi(frame, lease) },
        ),
        kind => panic!("bundled WPE returned unknown frame kind {kind}"),
    }
}

/// # Safety
///
/// `frame` must be a live DMA-BUF bridge frame whose descriptors this call takes
/// ownership of; see the module safety note.
unsafe fn dma_buf_from_abi(frame: &WaterWpeFrame, lease: Box<WpeFrameLease>) -> DmaBufFrame {
    assert_eq!(
        frame.modifier, DRM_FORMAT_MOD_LINEAR,
        "bundled WPE must negotiate DRM_FORMAT_MOD_LINEAR"
    );
    let n_planes = usize::try_from(frame.n_planes).expect("WPE plane count must fit usize");
    assert!(
        (1..=MAX_PLANES).contains(&n_planes),
        "WPE returned invalid DMA-BUF plane count {n_planes}"
    );
    assert_eq!(
        n_planes, 1,
        "WaterUI's WPE output contract requires one packed 32-bit plane"
    );
    let planes = (0..n_planes)
        .map(|index| {
            assert!(frame.fds[index] >= 0, "WPE DMA-BUF plane fd is invalid");
            DmaBufPlane {
                // SAFETY: bridge ABI call on the frame this lease owns; see the
                // module safety note.
                fd: unsafe { OwnedFd::from_raw_fd(frame.fds[index]) },
                offset: frame.offsets[index],
                stride: frame.strides[index],
            }
        })
        .collect();
    let rendering_fence = (frame.rendering_fence_fd >= 0)
        // SAFETY: bridge ABI call on the frame this lease owns; see the module
        // safety note.
        .then(|| unsafe { OwnedFd::from_raw_fd(frame.rendering_fence_fd) });
    // WPE hands over exactly the picture, so the frame needs no visible-size
    // narrowing; only a browser's padded shared image does.
    DmaBufFrame::new(
        frame.width,
        frame.height,
        DmaBufFormat::from_fourcc(frame.format),
        frame.modifier,
        planes,
        rendering_fence,
    )
    .with_lease(lease)
}

/// # Safety
///
/// `frame` must be a live shared-memory bridge frame whose mapping stays valid
/// and unchanged until `lease` is released; see the module safety note.
unsafe fn shm_from_abi(frame: &WaterWpeFrame, lease: Box<WpeFrameLease>) -> ShmFrame {
    // SAFETY: the caller guarantees the mapping outlives the lease it is given.
    unsafe {
        ShmFrame::new(
            frame.width,
            frame.height,
            frame.stride,
            shm_format_from_abi(frame.pixel_format),
            frame.data,
            frame.len,
            lease,
        )
    }
}

/// Exact WPE buffer ownership token.
pub struct WpeFrameLease {
    api: Arc<RuntimeApi>,
    token: *mut std::ffi::c_void,
    presented: bool,
    released: bool,
}

impl core::fmt::Debug for WpeFrameLease {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WpeFrameLease")
            .field("presented", &self.presented)
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

// SAFETY: completion is thread-safe in the bridge — it marshals every WPE object
// operation back onto the runtime's GMainContext, so the lease itself carries no
// thread affinity.
unsafe impl Send for WpeFrameLease {}

impl DmaBufLease for WpeFrameLease {
    /// Tells WPE the frame has been imported or copied by the backend.
    ///
    /// # Panics
    ///
    /// Panics when the frame was already presented.
    fn presented(&mut self) {
        assert!(!self.presented, "WPE frame was presented more than once");
        // SAFETY: bridge ABI call on the frame this lease owns; see the module
        // safety note.
        unsafe { (self.api.api.frame_presented)(self.token) };
        self.presented = true;
    }

    /// Returns the buffer to WPE after backend GPU work has completed.
    ///
    /// # Panics
    ///
    /// Panics when the frame was not presented first.
    fn release(mut self: Box<Self>, release_fence: Option<OwnedFd>) {
        assert!(self.presented, "WPE frame must be presented before release");
        let fd = release_fence.map_or(-1, IntoRawFd::into_raw_fd);
        // SAFETY: bridge ABI call on the frame this lease owns; see the module
        // safety note.
        unsafe { (self.api.api.frame_release)(self.token, fd) };
        self.released = true;
    }
}

impl Drop for WpeFrameLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if !self.presented {
            // SAFETY: bridge ABI call on the frame this lease owns; see the module
            // safety note.
            unsafe { (self.api.api.frame_presented)(self.token) };
        }
        // SAFETY: bridge ABI call on the frame this lease owns; see the module
        // safety note.
        unsafe { (self.api.api.frame_release)(self.token, -1) };
    }
}

#[cfg(test)]
mod tests {
    use crate::frame::ShmFormat;

    use super::{WPE_PIXEL_FORMAT_ARGB8888, shm_format_from_abi};

    #[test]
    fn the_only_pixel_format_wpe_defines_crosses_the_abi() {
        assert_eq!(
            shm_format_from_abi(WPE_PIXEL_FORMAT_ARGB8888),
            ShmFormat::Argb8888
        );
    }

    #[test]
    #[should_panic(expected = "unsupported WPE shared-memory pixel format 1")]
    fn an_unknown_pixel_format_fails_fast() {
        let _ = shm_format_from_abi(WPE_PIXEL_FORMAT_ARGB8888 + 1);
    }
}
