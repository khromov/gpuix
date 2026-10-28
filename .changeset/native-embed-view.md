---
'@gpuix/native': minor
---

A Rust app that owns its GPUI windows can render a retained tree inside them:
`GpuixView` and `GpuixView::new` are public, with the window title optional so
an embedded view never renames the host's window; `EventPayload` and its
siblings implement `serde::Serialize` on every target, so a host without napi
can hand events to its JS runtime as JSON; and `gpuix_native::init(cx)` and
`painted_bounds(id)` give such a host the app setup and the `getElementBounds`
answer that `GpuixRenderer` would otherwise own.
