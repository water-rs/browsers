//! The two kinds of platform buffer a browser page renders into.
//!
//! WPE Platform picks the kind, not this crate. A display with a DRM render node
//! renders into a `WPEBufferDMABuf`, which crosses as a
//! [`wgpu_external_frame::dma_buf::DmaBufFrame`] and imports into a texture with
//! no copy: importing a dma-buf is a general problem, so it lives in a crate that
//! knows nothing about WPE. A display without one — a container, a virtual
//! machine, a hosted CI runner — renders into a `WPEBufferSHM`, which crosses as
//! an [`ShmFrame`] and is uploaded from its mapping. Both are ordinary platform
//! buffers, and [`BrowserFrame`] is the choice between them.
//!
//! Neither type knows which engine produced it. What the engine owns is the
//! *lease* — the buffer belongs to its pool and has to go back once the
//! compositor has read it — and both kinds carry one:
//! [`DmaBufLease`], which `WpeFrameLease` implements for WPE.

use wgpu_external_frame::dma_buf::{DmaBufFormat, DmaBufFrame, DmaBufLease};

/// Bytes per pixel in the packed 32-bit formats both buffer kinds carry.
const BYTES_PER_PIXEL: u32 = 4;

/// One frame of page output, in whichever buffer kind the engine rendered it
/// into.
#[derive(Debug)]
pub enum BrowserFrame {
    /// A dma-buf, imported into a texture without a copy.
    DmaBuf(DmaBufFrame),
    /// A shared-memory mapping, uploaded into a texture.
    Shm(ShmFrame),
}

impl BrowserFrame {
    /// Whether the frame's contents are finished and may be read.
    ///
    /// A dma-buf carries a rendering fence to wait on; shared-memory pixels are
    /// written by the time the bridge hands them over, so they are always ready.
    #[must_use]
    pub fn is_render_ready(&self) -> bool {
        match self {
            Self::DmaBuf(frame) => frame.is_render_ready(),
            Self::Shm(_) => true,
        }
    }
}

/// The pixel format of a shared-memory frame, as `WPEPixelFormat` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmFormat {
    /// `WPE_PIXEL_FORMAT_ARGB8888`: 32 bits per pixel with the blue byte first
    /// in memory, which is the packing `DRM_FORMAT_ARGB8888` names.
    Argb8888,
}

impl ShmFormat {
    /// The packed 32-bit format this one is, so both buffer kinds answer the
    /// texture questions from one place.
    const fn packed(self) -> DmaBufFormat {
        match self {
            Self::Argb8888 => DmaBufFormat::Bgra8,
        }
    }

    /// The `wgpu` format a texture holding these pixels carries.
    #[must_use]
    pub const fn texture_format(self) -> wgpu::TextureFormat {
        self.packed().texture_format()
    }

    /// Whether a consumer must ignore the alpha channel and treat the frame as
    /// opaque.
    #[must_use]
    pub const fn force_opaque(self) -> bool {
        self.packed().force_opaque()
    }
}

/// One shared-memory frame: the engine's mapping, leased until it is released.
///
/// The pixels are borrowed rather than copied, so the mapping has to outlive the
/// lease this frame owns — which is what [`ShmFrame::new`] requires of its
/// caller.
#[derive(Debug)]
pub struct ShmFrame {
    width: u32,
    height: u32,
    stride: u32,
    format: ShmFormat,
    data: *const u8,
    len: usize,
    lease: Box<dyn DmaBufLease>,
}

impl ShmFrame {
    /// Describes a mapping the engine has finished drawing into.
    ///
    /// # Panics
    ///
    /// Panics when the description contradicts itself: a zero-sized frame, a
    /// stride shorter than one row of pixels, or a length that does not cover
    /// every row.
    ///
    /// # Safety
    ///
    /// `data` must address `len` initialized bytes that stay mapped, and stay
    /// unchanged, until `lease` is released.
    #[must_use]
    pub unsafe fn new(
        width: u32,
        height: u32,
        stride: u32,
        format: ShmFormat,
        data: *const u8,
        len: usize,
        lease: Box<dyn DmaBufLease>,
    ) -> Self {
        assert!(
            width > 0 && height > 0,
            "shared-memory frame must not be zero-sized"
        );
        assert!(!data.is_null(), "shared-memory frame carries no pixels");
        let row = width
            .checked_mul(BYTES_PER_PIXEL)
            .expect("shared-memory row length exceeds u32");
        assert!(
            stride >= row,
            "shared-memory stride {stride} is shorter than a row of {width} pixels"
        );
        let mapped = usize::try_from(stride)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(stride, height)| stride.checked_mul(height))
            .expect("shared-memory mapping exceeds usize");
        assert!(
            len >= mapped,
            "shared-memory frame holds {len} bytes for {height} rows of {stride}"
        );
        Self {
            width,
            height,
            stride,
            format,
            data,
            len,
            lease,
        }
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes between adjacent rows.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// The pixel format the mapping holds.
    #[must_use]
    pub const fn format(&self) -> ShmFormat {
        self.format
    }

    /// The mapping itself, row-major and [`Self::stride`] bytes per row.
    #[must_use]
    pub const fn pixels(&self) -> &[u8] {
        // SAFETY: `new` was given a pointer to `len` initialized bytes that stay
        // mapped and unchanged until the lease this frame owns is released, and
        // the lease is released by consuming the frame.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }

    /// Tells the engine the frame has been read.
    ///
    /// # Panics
    ///
    /// Panics when the frame was already presented.
    pub fn presented(&mut self) {
        self.lease.presented();
    }

    /// Returns the buffer to the engine.
    ///
    /// There is no fence to pass: the upload copies out of the mapping before
    /// this call, and a shared-memory buffer carries no GPU work to wait on.
    ///
    /// # Panics
    ///
    /// Panics when the frame was not presented first.
    pub fn release(self) {
        DmaBufLease::release(self.lease, None);
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wgpu_external_frame::dma_buf::DmaBufLease;

    use super::{BYTES_PER_PIXEL, ShmFormat, ShmFrame};

    #[derive(Debug)]
    struct CountingLease {
        presented: Arc<AtomicUsize>,
        released: Arc<AtomicUsize>,
    }

    impl DmaBufLease for CountingLease {
        fn presented(&mut self) {
            self.presented.fetch_add(1, Ordering::Relaxed);
        }

        fn release(self: Box<Self>, release_fence: Option<OwnedFd>) {
            assert!(
                release_fence.is_none(),
                "a shared-memory frame has no fence"
            );
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn frame(pixels: &[u8], stride: u32, lease: Box<dyn DmaBufLease>) -> ShmFrame {
        // SAFETY: `pixels` outlives the frame, which this test drops first, and
        // nothing writes to it while the frame holds it.
        unsafe {
            ShmFrame::new(
                4,
                2,
                stride,
                ShmFormat::Argb8888,
                pixels.as_ptr(),
                pixels.len(),
                lease,
            )
        }
    }

    #[test]
    fn shm_formats_map_onto_the_texture_contract_the_dma_buf_path_produces() {
        let format = ShmFormat::Argb8888;
        assert_eq!(
            format.texture_format(),
            wgpu::TextureFormat::Bgra8Unorm,
            "WPE_PIXEL_FORMAT_ARGB8888 is the packing DRM_FORMAT_ARGB8888 names"
        );
        assert!(
            !format.force_opaque(),
            "an ARGB frame's alpha channel is the page's own"
        );
        assert_eq!(
            format.texture_format().block_copy_size(None),
            Some(BYTES_PER_PIXEL),
            "the mapping is packed 32-bit, which is what the stride check assumes"
        );
    }

    #[test]
    fn a_padded_mapping_keeps_its_stride_and_its_lease() {
        let presented = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let pixels = vec![0x7fu8; 64];
        let mut shm = frame(
            &pixels,
            32,
            Box::new(CountingLease {
                presented: Arc::clone(&presented),
                released: Arc::clone(&released),
            }),
        );
        assert_eq!(shm.stride(), 32, "a padded row keeps the engine's stride");
        assert_eq!(shm.pixels().len(), pixels.len());
        shm.presented();
        shm.release();
        assert_eq!(presented.load(Ordering::Relaxed), 1);
        assert_eq!(released.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[should_panic(expected = "shared-memory frame holds 32 bytes for 2 rows of 32")]
    fn a_mapping_shorter_than_its_rows_fails_fast() {
        let presented = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let pixels = vec![0u8; 32];
        let _ = frame(
            &pixels,
            32,
            Box::new(CountingLease {
                presented,
                released,
            }),
        );
    }

    #[test]
    #[should_panic(expected = "shared-memory stride 8 is shorter than a row of 4 pixels")]
    fn a_stride_shorter_than_a_row_fails_fast() {
        let presented = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let pixels = vec![0u8; 64];
        let _ = frame(
            &pixels,
            8,
            Box::new(CountingLease {
                presented,
                released,
            }),
        );
    }
}
