# waterui-browser-cef

Shared Chromium Embedded Framework runtime for WaterUI WebView and Chromium components.

## Real-engine tests

`tests/real_engine.rs` drives a genuine Chromium and asserts on what crosses the
web view bridge in both directions. It sits behind the `real-engine` feature,
and it runs under `cargo test` rather than `cargo nextest run` because CEF
requires the browser process to be `main`:

```sh
cargo test -p waterui-browser-cef --features real-engine --test real_engine
```

The checks are the same on every platform. What a platform has to provide in
order to run them is not.

- **macOS** runs all of them. The checks stage an application bundle with its
  Chromium helpers and re-run from inside it.
- **Linux** runs them on a machine with a GPU, and reports a skip on a machine
  without one. Every CEF browser here is windowless and hands its frames over as
  a shared texture, which on Linux is a DMA-BUF exported by Chromium's GPU
  process; a machine whose only adapter is Mesa's llvmpipe has no GPU allocation
  to export, and Chromium refuses to create the browser at all. So the checks
  ask `wgpu` for the same adapter the realization would draw on, before CEF is
  initialized, and say on one line why they stopped when that adapter is a
  software rasterizer. Hosted CI runners have no GPU and report exactly that;
  the runtime is still staged and the checks still re-run from it.

The asymmetry is a property of the machine rather than a choice in the backend.
There is no software-rendering path here to fall back to — the realization is
GPU-only by design — and a Linux host with a GPU runs every check exactly as
macOS does.
