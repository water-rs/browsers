use std::os::fd::{BorrowedFd, OwnedFd};

use cef::{AcceleratedPaintInfo, ColorType, PaintElementType, Rect};
use waterui_graphics::gpu::FrameOutput;
use wgpu_external_frame::dma_buf::{DmaBufFormat, DmaBufFrame, DmaBufImporter, DmaBufPlane};

use super::publisher::{FramePublisher, PlainAllocator};
use crate::{AcceleratedFrameSink, CefPopupRect};

pub(super) struct LinuxFrameSink {
    importer: DmaBufImporter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    publisher: FramePublisher<PlainAllocator>,
}

impl AcceleratedFrameSink for LinuxFrameSink {
    fn import(
        &self,
        element: PaintElementType,
        _dirty_rects: &[Rect],
        frame: &AcceleratedPaintInfo,
    ) {
        assert_eq!(
            frame.plane_count, 1,
            "CEF Linux accelerated paint must provide one packed DMA-BUF plane"
        );
        let size = &frame.extra.coded_size;
        let width = u32::try_from(size.width).expect("CEF DMA-BUF width must be positive");
        let height = u32::try_from(size.height).expect("CEF DMA-BUF height must be positive");
        let plane = &frame.planes[0];
        assert!(plane.fd >= 0, "CEF DMA-BUF file descriptor is invalid");
        // SAFETY: `borrow_raw` requires the descriptor to be open and to stay
        // open for the borrow's lifetime. CEF owns this descriptor and keeps it
        // valid for the duration of the `on_accelerated_paint` callback this
        // runs inside, which is exactly the scope of `borrowed`; it is asserted
        // non-negative just above. The borrow is only used to duplicate the
        // descriptor into an `OwnedFd`, so nothing outlives the callback and
        // CEF's own close is unaffected.
        let borrowed = unsafe { BorrowedFd::borrow_raw(plane.fd) };
        let fd: OwnedFd = borrowed
            .try_clone_to_owned()
            .expect("failed to duplicate CEF DMA-BUF file descriptor");
        let format = if frame.format == ColorType::BGRA_8888 {
            DmaBufFormat::Bgra8
        } else if frame.format == ColorType::RGBA_8888 {
            DmaBufFormat::Rgba8
        } else {
            panic!("CEF returned unsupported Linux accelerated color format")
        };
        let mut borrowed_frame = DmaBufFrame::new(
            width,
            height,
            format,
            frame.modifier,
            vec![DmaBufPlane {
                fd,
                offset: u32::try_from(plane.offset).expect("CEF DMA-BUF offset exceeds u32"),
                stride: plane.stride,
            }],
            None,
        );
        // Only the region that holds the page: Chromium may allocate the shared
        // image at a coded size with alignment padding, and presenting that edge
        // to edge stretches the page and draws the gutter.
        let visible = &frame.extra.visible_rect;
        if let (Ok(visible_width), Ok(visible_height)) =
            (u32::try_from(visible.width), u32::try_from(visible.height))
            && visible_width <= width
            && visible_height <= height
        {
            borrowed_frame = borrowed_frame.with_visible_size(visible_width, visible_height);
        }
        self.publisher.paint(
            element,
            borrowed_frame.visible_size(),
            format.texture_format(),
            |destination| {
                let import = self.importer.copy_into(&mut borrowed_frame, destination);
                // The copy and the barriers around it go ahead of the encoder in
                // one submission. CEF takes the DMA-BUF back when this callback
                // returns and Vulkan does not order Chromium's next write behind
                // our read, so the copy completes before the callback does.
                let submission = self.queue.submit(
                    import
                        .command_buffers
                        .into_iter()
                        .chain([import.encoder.finish()]),
                );
                self.device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(submission),
                        timeout: None,
                    })
                    .expect("the CEF DMA-BUF copy failed");
                borrowed_frame.presented();
                drop(import.guard);
            },
        );
    }

    fn set_popup_rect(&self, rect: Option<CefPopupRect>) {
        self.publisher.set_popup_rect(rect);
    }
}

pub(super) fn make_frame_sink(output: FrameOutput) -> LinuxFrameSink {
    let shared = output.shared_device();
    assert_eq!(
        shared.adapter.get_info().backend,
        wgpu::Backend::Vulkan,
        "CEF DMA-BUF composition requires WaterUI's Vulkan backend"
    );
    let importer = DmaBufImporter::new(&shared.device, &shared.queue, &shared.adapter);
    let device = shared.device.clone();
    let queue = shared.queue.clone();
    LinuxFrameSink {
        importer,
        device: device.clone(),
        queue,
        publisher: FramePublisher::new(output, PlainAllocator { device }),
    }
}
