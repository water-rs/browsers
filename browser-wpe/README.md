# waterui-browser-wpe

Bundled WPE WebKit runtime for WaterUI on Linux.

## Frames

The runtime hands each rendered frame over as a platform buffer, and WPE Platform
picks which kind from what the host can do:

- **dma-buf** on a host with a DRM render node. The frame crosses as a
  `DmaBufFrame` and is imported into a `wgpu` texture without a copy.
- **shared memory** on a host without one — a container, a virtual machine, a
  hosted CI runner. The frame crosses as an `ShmFrame` and is uploaded into the
  same texture from its mapping.

Neither is a fallback for the other, and neither is an error: the compositing
view accepts both and produces one texture contract. Both are leased — the buffer
belongs to WPE's own pool and is returned once it has been read.
