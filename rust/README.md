# Gamescope in Rust

This directory contains a compatibility-first Rust reimplementation beside the
production C++ compositor.

The workspace now contains:

- `gamescope-protocols`: client and server bindings generated directly from all
  12 checked-in protocol XML files;
- `gamescope-core`: hardware-independent scaling, timing, swapchain, control,
  input-method, and action-binding policy;
- `gamescope-wayland-server`: embeddable dispatch for every Gamescope global
  used by the compositor frontend;
- `gamescope-compositor`: the native `gamescope-rs` executable.

`gamescope-rs` is a working nested and atomic-DRM compositor. It provides core surfaces and
subsurfaces, shm and dma-buf buffers, xdg shell, layer shell, an output and seat,
clipboard/DnD, pointer constraints, relative pointer, fractional scale,
viewporter, presentation feedback, Xwayland, and the Gamescope-specific
protocols. It renders Wayland and X11 clients with Smithay's nested GLES backend.

On a free VT/seat, the hardware backend owns the selected DRM primary node,
uses atomic modesets, GBM/EGL, kernel-tested primary/overlay/cursor plane
assignment, direct scanout, and GLES composition fallback:

```sh
target/release/gamescope-rs --drm -O 'eDP-1,*' --adaptive-sync -- your-game
```

`--backend auto` retains the nested backend under an existing desktop and
selects DRM when no host display is present. `--drm-device`, `--output-refresh`,
and `--disable-direct-scanout` provide explicit hardware control.

Build and run it from the repository root:

```sh
cargo build --release -p gamescope-compositor
target/release/gamescope-rs -w 1280 -h 720 -r 60 -- your-game
```

Steam mode now has an active implementation rather than a compatibility no-op:

```sh
target/release/gamescope-rs --steam --xwayland-count 2 -- steam -gamepadui
```

It exports the Gamescope/Steam capability environment, assigns the primary
Steam display plus `STEAM_GAME_DISPLAY_n`, implements dynamic Xwayland
creation/destruction, watches Steam's X11 app/overlay/input properties, applies
base/transient/overlay/Remote Play policy, and publishes the focusable/focused
window and app-ID root properties. Screen scale/magnification and the
`GAMESCOPE_FPS_LIMIT` → WSI limiter-file channel are live as well.
Steam's connector-force, mode-nudge, per-screen dynamic-refresh,
composite-force, VRR, display identity, and active refresh feedback paths are
connected to the atomic backend.
`--expose-wayland` opts the child into a
Wayland session; the faithful default is an X11 session with only
`GAMESCOPE_WAYLAND_DISPLAY` exposing the private compositor socket.

The existing Vulkan WSI layer in `layer/` is intentionally reused. Child
processes receive `ENABLE_GAMESCOPE_WSI=1`, `GAMESCOPE_WAYLAND_DISPLAY`, and the
appropriate Xwayland `DISPLAY`; swapchain/HDR/present metadata is consumed by
the Rust server and presentation timing is sent back to the layer.

The latency-critical ownership boundaries follow Gamescope: the Wayland and
libinput loop exchanges snapshots with a `gamescope-xwm` actor for blocking X11
property work and a `gamescope-drm` actor for rendering, atomic tests/commits,
page flips, connector scans, DPMS, and mode changes. Both actor mailboxes
coalesce obsolete frame/control publications instead of blocking input.

Run validation with:

```sh
cargo test --workspace --all-features
```

The integration tests use real Wayland socket pairs. They cover registry
negotiation, core surface commits, Vulkan swapchain timing, private input-method
transactions, Steam focus/role policy, Steam launch options, backend command
handoff, and asynchronous replies without needing a running desktop.

This is not yet a feature-complete replacement for the production Gamescope
binary. Atomic DRM/KMS, direct scanout, explicit-sync timelines, Steam/Xwayland,
and the ordinary nested path are operational. Gamescope's Vulkan compute
shaders and full color/HDR pipeline, DRM leasing, PipeWire production,
screenshots, ReShade compilation, Lua, SDL, OpenVR, and production headless
capture still use the C++ implementation. See
[`docs/rust-reimplementation.md`](../docs/rust-reimplementation.md) for the
coverage matrix and remaining backend work.
