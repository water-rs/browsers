//! Whether this machine has a GPU the Linux checks can be run on.
//!
//! Every CEF browser here is created with `shared_texture_enabled`, and a
//! shared texture is a handle to a GPU allocation Chromium's GPU process
//! exports and the application imports as a DMA-BUF. Chromium answers
//! `browser_host_create_browser_sync` with null when it cannot produce one, and
//! a hosted runner with no GPU is exactly such a machine: the checks reached
//! their first page, panicked there, and said nothing about why.
//!
//! So the question is asked before CEF is, and asked the way the realization
//! itself asks it. `waterui-graphics` requests the adapter every GPU surface
//! runs on with `force_fallback_adapter: false`, and `wgpu-external-frame`'s
//! `DmaBufImporter` requires that adapter to be Vulkan or EGL/GLES. Mesa
//! answers a machine with no GPU with llvmpipe: a real adapter, of device type
//! `Cpu`, that rasterizes on the processor and has no GPU allocation to export.
//! That device type is the signal.
//!
//! This is a probe, not a fallback. Nothing here selects a software path or
//! relaxes what the checks prove; it decides only whether they can run on this
//! machine at all, and it is compiled on Linux alone — macOS has a GPU, and the
//! leg there proves the checks.

/// Why this machine cannot run the real-engine checks, or `None` when it can.
pub fn unusable_reason() -> Option<String> {
    // The same instance and the same request `waterui-graphics` makes for the
    // GPU surface the realization draws into: asking a different question would
    // answer about a different machine.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::all();
    let instance = wgpu::Instance::new(descriptor);
    let adapter =
        futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }));
    let information = match adapter {
        Ok(adapter) => adapter.get_info(),
        Err(error) => return Some(format!("this machine has no wgpu adapter at all: {error}")),
    };
    if information.device_type == wgpu::DeviceType::Cpu {
        return Some(format!(
            "this machine's only adapter is the software rasterizer {} on {:?}, which has no GPU \
             allocation for CEF to export as a shared texture",
            information.name, information.backend
        ));
    }
    if !matches!(
        information.backend,
        wgpu::Backend::Vulkan | wgpu::Backend::Gl
    ) {
        return Some(format!(
            "this machine's adapter {} runs on {:?}, and importing a CEF DMA-BUF needs Vulkan or \
             EGL/GLES",
            information.name, information.backend
        ));
    }
    None
}
