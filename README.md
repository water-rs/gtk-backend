# waterui-gtk

GTK4 backend for [WaterUI](https://github.com/water-rs/waterui), mapping
WaterUI views to native GTK4 widgets.

## Architecture

- Native GTK4 widgets (`Label`, `Button`, `Switch`, …) where a platform
  primitive exists; WaterUI composers cover the rest.
- Layout is computed by `waterui-layout`; GTK measures and places widgets.
- Reactivity is fine-grained `nami` signals — widget properties update
  directly, never whole-subtree rebuilds.
- `GpuSurface` renders through `GtkGLArea` with a wgpu device adopted from
  the widget's GL context (`wgpu-hal` external adapter) — no extra surface
  layer, no Wayland/X11 specifics.

## Platform support

Linux only. On every other host the crate compiles to an empty library so
cross-platform builds keep working.

## Testing

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
xvfb-run -a cargo nextest run          # GTK tests need a display
```

CI runs the same gates on `ubuntu-latest` with llvmpipe (software GL 4.5)
providing the GL context GPU-path tests need.
