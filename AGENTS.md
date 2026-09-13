# AGENTS.md

GTK4 backend for WaterUI. Issues and the design constraints that govern every
WaterUI feature live in `water-rs/waterui` (`AGENTS.md` there); this file
carries only what is specific to this repository.

## Workflow

- **Finding a problem → GitHub issue in this repo. Solving it → pull request
  to `dev`.** One PR resolves one issue or lands one discrete fix; the PR body
  links the issue (`Fixes #N`). Never push `dev` directly.
- The crate is Linux-only: every module except `shape_geometry` is behind
  `cfg(target_os = "linux")`, and on other hosts the crate compiles to an
  empty library. Keep that boundary — host-portable code must not accrete GTK
  dependencies.
- Lint bar: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo nextest run`. No new warnings, clippy warnings included.
- `Cargo.lock` is committed; CI builds the locked graph.

## Testing

GTK tests need a display. Headless runs go through `xvfb-run`; llvmpipe
(mesa) provides a real GL 4.5 context, which is what the `GpuSurface` /
filter tests require — GTK4's GL pipeline refuses contexts below GL 3.3 and
the filter pipeline's compute passes need 4.3/ES 3.1.

Focus tests additionally need the window's toplevel to hold real input focus,
which bare Xvfb never grants — a minimal window manager (openbox) must be
running inside the session, and tests that present a toplevel belong to the
`windowed` serial test-group in `.config/nextest.toml` so two of them cannot
steal each other's focus.

```
xvfb-run -a dbus-run-session -- sh -c 'openbox & sleep 2; cargo nextest run'
```

Do not stub out `gtk4::init` failures or skip tests when a display is missing
— a test that silently passes without rendering is a false positive; make the
environment provide the display instead.

## Dependencies

This crate builds against the **published** `waterui-*` crates, not the
monorepo's path dependencies. When a needed API only exists on waterui's
unreleased `dev`, the change belongs there first — land it in waterui, let it
publish, then depend on the released version. Do not point this crate at git
branches of the monorepo to shortcut that.

`glow` must stay on the version `wgpu-hal` links: the backend hands
`glow::NativeFramebuffer` handles to `wgpu::hal::gles`, and a version skew
makes them different types.
