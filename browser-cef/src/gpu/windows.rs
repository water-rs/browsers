use std::ptr::NonNull;

use cef::{AcceleratedPaintInfo, ColorType, PaintElementType, Rect};
use waterui_graphics::gpu::FrameOutput;
use wgpu_external_frame::shared_handle::SharedHandleFrame;

use super::publisher::{FramePublisher, PlainAllocator, submit_copy};
use crate::{AcceleratedFrameSink, CefPopupRect};

pub(super) struct WindowsFrameSink {
    device: wgpu::Device,
    queue: wgpu::Queue,
    publisher: FramePublisher<PlainAllocator>,
}

impl AcceleratedFrameSink for WindowsFrameSink {
    fn import(
        &self,
        element: PaintElementType,
        _dirty_rects: &[Rect],
        frame: &AcceleratedPaintInfo,
    ) {
        let handle = NonNull::new(frame.shared_texture_handle)
            .expect("CEF accelerated paint returned a null D3D shared handle");
        let size = &frame.extra.coded_size;
        let width = u32::try_from(size.width).expect("CEF D3D texture width must be positive");
        let height = u32::try_from(size.height).expect("CEF D3D texture height must be positive");
        let format = if frame.format == ColorType::BGRA_8888 {
            wgpu::TextureFormat::Bgra8Unorm
        } else if frame.format == ColorType::RGBA_8888 {
            wgpu::TextureFormat::Rgba8Unorm
        } else {
            panic!("CEF returned unsupported Windows accelerated color format")
        };
        // SAFETY: `import` is the accelerated paint callback, so the handle CEF
        // put in the paint info is valid in this process for exactly this call,
        // which is when it is duplicated. The extent and format come out of the
        // same paint info and describe the resource behind it.
        let shared = unsafe { SharedHandleFrame::duplicate(handle, width, height, format) };
        let source = shared.import(&self.device);
        // Only the visible region: `coded_size` may carry alignment padding, and
        // presenting the padded texture edge to edge stretches the page and
        // draws the gutter.
        let visible = &frame.extra.visible_rect;
        let extent = (
            u32::try_from(visible.width).unwrap_or(width).min(width),
            u32::try_from(visible.height).unwrap_or(height).min(height),
        );
        self.publisher
            .paint(element, extent, format, |destination| {
                submit_copy(&self.device, &self.queue, &source, destination, extent);
            });
    }

    fn set_popup_rect(&self, rect: Option<CefPopupRect>) {
        self.publisher.set_popup_rect(rect);
    }
}

pub(super) fn make_frame_sink(output: FrameOutput) -> WindowsFrameSink {
    assert_eq!(
        output.shared_device().adapter.get_info().backend,
        wgpu::Backend::Dx12,
        "CEF shared D3D texture composition requires WaterUI's Direct3D 12 backend"
    );
    let device = output.device().clone();
    let queue = output.queue().clone();
    WindowsFrameSink {
        publisher: FramePublisher::new(
            output,
            PlainAllocator {
                device: device.clone(),
            },
        ),
        device,
        queue,
    }
}
