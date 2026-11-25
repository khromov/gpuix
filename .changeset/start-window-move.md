---
"@gpuix/native": minor
---

Add `startWindowMove()` to the renderer, translated from GPUI's
`Window::start_window_move`. A custom title bar can now drag the window on
macOS, Wayland and X11. Call it on the first mouse move after a mouse down on
the drag area, while the button is still held. It does nothing on Windows,
where GPUI does not implement it.
