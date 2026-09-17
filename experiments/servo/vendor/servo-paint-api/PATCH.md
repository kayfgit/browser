# Local patch to servo-paint-api 0.5.0

Source: published `servo-paint-api` crate version 0.5.0 (MPL-2.0).
Original copyright/license headers are retained in each source file.

The only source change is in `SurfmanRenderingContext::new`: make the newly
created context current before loading GL entry points or constructing glow's
context. On Windows WGL, Surfman restores the previous current context after
creation. Without this change, the standalone embedder panics with
`Reading GL_VERSION failed` inside glow before it can attach a window surface.
If activation fails, explicitly destroy the native context before returning.

Both the main workspace's optional Servo provider and the lab use this patch.
Remove it when updating to an upstream release that fixes this initialization
order. Both native smoke tests exercise the affected path. No shared Cargo
registry files were modified.
