# A compatibility-first Rust reimplementation of Gamescope

## Scope and fidelity

Gamescope is not just a Wayland compositor. In this checkout it is a micro-
compositor, X11 window manager, Xwayland host, Vulkan compute compositor,
DRM/KMS display server, nested Wayland client, input router, frame scheduler,
PipeWire producer, and Vulkan WSI layer. The production implementation is about
40,000 lines before the bundled ReShade sources. A faithful rewrite therefore
needs staged replacement with differential tests; a single large translation
would make behavioral drift almost impossible to diagnose.

For this port, compatibility is ordered as follows:

1. Protocol XML defines interface names, opcodes, versions, argument order, and
   enum values. Rust bindings are generated from the same checked-in XML.
2. Observable C++ behavior defines request semantics, state lifetime, focus and
   selection policy, frame ordering, and presentation timing.
3. Backend results must match for the same capabilities. The implementation
   keeps Gamescope's XWM/Main, Wayland/input, and DRM ownership boundaries so a
   blocking X11 round trip or modeset cannot stall critical input dispatch.
4. Accidental behavior that contradicts the XML is recorded and tested before
   deciding whether it needs an explicit legacy-compatibility mode.

## What the current C++ architecture does

| Area | Primary implementation | Responsibility |
|---|---|---|
| Process and options | `src/main.cpp` | CLI, environment, child process, privileges, resolution defaults, backend selection |
| Wayland/Xwayland | `src/wlserver.cpp`, `src/ime.cpp`, `src/WaylandServer/` | Globals, surfaces, commit capture, seats, Xwayland instances, explicit sync, private protocols |
| Window policy | `src/steamcompmgr.cpp` | X11 properties, focus, overlays, commit queues, layer selection, scaling, frame limiting, screenshots |
| Rendering | `src/rendervulkan.cpp`, `src/shaders/` | dmabuf import, compute composition, FSR, NIS, blur, color conversion, screenshots |
| Display backends | `src/Backends/` | Atomic DRM/KMS and liftoff, nested Wayland, SDL, OpenVR, headless output |
| Scheduling | `src/vblankmanager.cpp`, `src/Timeline.cpp` | vblank prediction, latch deadlines, explicit acquire/release timeline points |
| Color | `src/color_helpers.cpp`, backend code | EOTFs, gamut mapping, LUTs, HDR metadata, mura correction |
| Capture | `src/pipewire.cpp` | PipeWire stream negotiation and NV12 output |
| WSI integration | `layer/` | Vulkan layer that reports swapchain and presentation metadata to Gamescope |

The critical data path is:

```text
game -> Xwayland/Wayland surface commit -> acquire synchronization
     -> focus and layer policy -> direct-scanout decision or Vulkan composition
     -> backend present -> presentation feedback/release synchronization
```

The Rust design keeps this path explicit instead of sharing mutable globals
across the Wayland, X11, render, and vblank threads.

## Rust crate boundaries

```text
gamescope-protocols       XML-generated client/server wire types
          |
gamescope-wayland-server request dispatch, resource lifetimes, event delivery
          |
gamescope-core            pure policy and protocol state machines
          |
gamescope-compositor      nested GLES compositor, input, Xwayland, presentation
          +---- renderer-vulkan and synchronization
          +---- backend-drm / backend-wayland / SDL / OpenVR
          +---- PipeWire, color, screenshots, scripting
```

The nested compositor and atomic DRM rows are implemented in
`gamescope-compositor`. Backend work is passed out of Wayland dispatch as owned
commands or coalesced snapshots. Blocking X11 property traffic is owned by
`gamescope-xwm`; GBM/EGL rendering, atomic tests/commits, connector operations,
and page flips are owned by `gamescope-drm`. Asynchronous protocol results
retain weak reply tokens, so a client disconnect cannot leave a dangling
resource pointer.

### Implemented now

- `gamescope-protocols` generates both client and server APIs from every local
  XML file: all Gamescope protocols, frog color management, the bundled color
  management snapshot, layer shell, and xdg toplevel icon.
- `gamescope-core` ports and tests:
  - auto, integer, fit, fill, and stretch scale calculations;
  - hertz, millihertz, and refresh-cycle conversions;
  - reduced ratio parsing behavior;
  - split 64-bit Wayland time words;
  - persistent swapchain feedback and one-shot commit metadata;
  - Gamescope's HDR metadata acceptance rule;
  - control feature ordering, refresh flags, display power, and performance
    request fan-out;
  - input-method serial and double-buffer behavior;
  - action binding normalization, exact-set matching, arming, and one-shot
    execution.
- `gamescope-wayland-server` implements actual globals for:
  - `gamescope_action_binding_manager` and `gamescope_action_binding` v1;
  - `gamescope_control` v6;
  - `gamescope_pipewire` v1 when a node ID is available;
  - `gamescope_private` v1;
  - `gamescope_reshade` v1;
  - `gamescope_swapchain_factory_v2` and `gamescope_swapchain` v1;
  - `gamescope_input_method_manager` v3;
  - legacy `gamescope_xwayland` v1.
- `gamescope-compositor` builds the `gamescope-rs` executable with:
  - compositor v5, subsurfaces, shm v2, xdg shell, and layer shell v4;
  - output/xdg-output, seat, keyboard, pointer, clipboard, and DnD;
  - viewporter, fractional scaling, relative pointer, pointer constraints,
    single-pixel buffers, and presentation feedback;
  - dma-buf v4 feedback when an EGL render node is available, with the v3
    format-list fallback otherwise;
  - libseat/udev/libinput hardware operation with atomic-only DRM device
    creation, connector/CRTC selection, EDID identity, `-O` priority and Steam
    force-internal/mode-nudge handling, hotplug, pause/resume, and DPMS;
  - GBM/EGL GLES output with kernel-tested primary, overlay, and cursor plane
    assignment, direct scanout, composition fallback, and a runtime
    composite-force switch;
  - dma-buf scanout feedback tranches, linux-drm-syncobj acquire blockers and
    release points, actual page-flip sequence/timestamp presentation feedback,
    VRR capability/control, and same-resolution dynamic mode switching;
  - dedicated DRM and X11-policy actors with a latest-frame mailbox, so atomic
    work and X11 replies cannot block Wayland/libinput dispatch;
  - nested GLES composition of surface trees and output scaling;
  - Xwayland process/XWM lifecycle, fullscreen mapping, and both legacy and
    swapchain content overrides;
  - Steam mode capability/session environment and reuse of the existing Vulkan
    WSI layer through `ENABLE_GAMESCOPE_WSI`;
  - multiple initial Xwayland servers, `STEAM_GAME_DISPLAY_n`, and Steam's
    dynamic `GAMESCOPE_CREATE_XWAYLAND_SERVER`/destroy/feedback contract;
  - XRes PID lookup and Steam reaper ancestry app-ID discovery, plus live
    `STEAM_GAME`, `STEAM_BIGPICTURE`, overlay, external-overlay, Remote Play,
    opacity, and input-focus property tracking;
  - steamcompmgr-compatible base/transient selection, Gamepad UI/notification/
    external overlay composition, mode-1/mode-2 input routing, and the Steam UI
    maximum-height rule;
  - root-window focus controls and focusable/focused app, window, PID, display,
    capability, refresh, server-ID, process-ID, and input-counter feedback;
  - fixed-point Steam screen scale/magnification with inverse pointer mapping,
    and the `GAMESCOPE_FPS_LIMIT` limiter file consumed by the reused WSI layer;
  - action-binding filtering and private input-method keyboard/pointer input;
  - swapchain metadata consumed atomically with surface commits and Vulkan
    past-presentation/refresh-cycle events after display.
- Real socket-pair tests check protocol negotiation, core surface commits,
  Vulkan timing, IME transactions, command handoff, and asynchronous responses.
- Live smoke tests under a headless Weston host ran `weston-simple-shm`,
  enumerated the advertised globals with `wayland-info`, started Xwayland, and
  ran `glxgears` through the XWM/render loop. A two-Xwayland Steam-mode test
  verified both server IDs, child session/capability variables, app-ID focus
  feedback, XRes PIDs, and runtime Xwayland create/destroy feedback against real
  X11 servers.

### Not implemented yet

The Rust executable is not yet a production replacement for every Gamescope
subsystem. These paths still belong to C++:

- hardware HDR/color properties and metadata, DRM leasing, async tearing, and
  multi-output simultaneous composition (the current DRM backend selects one
  Gamescope-priority connector at a time);
- Gamescope's Vulkan compute renderer and exact FSR/NIS/blur shader/color
  pipeline (the existing Vulkan WSI layer is reused and Smithay GLES supplies
  composition fallback);
- SDL, OpenVR, production headless capture, PipeWire production, screenshots,
  color/LUT/mura processing, ReShade compilation, and Lua scripting;
- specialized per-title steamcompmgr workarounds, Wine HWND-style heuristics,
  VR-overlay forwarding, and SteamOS virtual-connector policy.

The Vulkan WSI layer is deliberately not being rewritten. The existing `layer/`
implementation can use the Rust swapchain protocol endpoint.

## Protocol compatibility matrix

The distinction between *server* and *nested client* matters. `protocol/meson.build`
generates both sides for all XML, but that does not mean Gamescope advertises
every protocol.

| Protocol family | C++ role in this checkout | Rust status |
|---|---|---|
| Core compositor and output | Server, compositor v5 and one virtual output per Xwayland instance | Implemented with one shared nested output and per-Xwayland server identity |
| `wl_shm` | Server v2 through renderer setup | Implemented |
| linux-dmabuf | Server v4 when renderer supports dmabuf; nested client v3 | Implemented with v4 feedback/v3 fallback |
| xdg shell | Server v3; nested client | Implemented server-side |
| presentation-time | Server v1 with Gamescope timing; nested client | Implemented server-side |
| pointer constraints and relative pointer | Server and nested client | Implemented server-side |
| primary selection | Nested client only in the code inspected | Binding available upstream; not wired |
| viewporter, fractional scale, single-pixel buffer | Nested client only in the code inspected | Advertised by Rust server for broad client compatibility |
| linux-drm-syncobj | Conditional server when the backend supports explicit sync | Advertised on capable DRM devices; acquire timeline blockers and release-point signaling implemented |
| layer shell v1 | Server v4 | Implemented and configured to the nested output |
| xdg toplevel icon | Nested client | Local Rust binding generated; behavior not wired |
| color-management-v1 | Nested client | Local Rust binding generated; behavior not wired |
| frog color management | Nested client fallback | Local Rust binding generated; behavior not wired |
| Gamescope action binding | Server | Implemented and wire-tested |
| Gamescope control | Server v6 | Implemented and wire-tested; physical display info rebroadcast, refresh-cycle mode/pacing and display power reach DRM; screenshot/look/render-effect adapters remain |
| Gamescope input method | Server v3 | Implemented with generated IME keymaps and pointer dispatch |
| Gamescope PipeWire discovery | Conditional server | Implemented and wire-tested; producer is not ported |
| Gamescope private | Server | Implemented and wire-tested |
| Gamescope ReShade | Server | Implemented and wire-tested; compiler/renderer is not ported |
| Gamescope swapchain | Server | Implemented through commit and presentation timing |
| Gamescope Xwayland override | Legacy server | Primary-server fallback preserved; Xwayland-owned clients resolve their instance, while modern swapchain overrides carry an explicit server ID |

## Compatibility details already captured by tests

- Swapchain feedback persists across commits. Present ID, desired presentation
  time, and current Vulkan present mode reset after one commit.
- HDR metadata is ignored until swapchain feedback exists. It is also discarded
  when `max_cll` or `max_fall` is zero, or when both white-point coordinates are
  zero.
- A stale input-method serial does not clear pending text or action.
- Input-method wheel values are divided by 120, and synthetic pointer timestamps
  increment once per request.
- Display sleep gives the sleep bit precedence when a malformed request includes
  both sleep and wake.
- Integer scaling floors only scales above 1.0; downscaling remains fractional.
- Auto mode applies its maximum before physical-output scaling, and global
  overscan/zoom is applied afterward.
- Refresh conversion deliberately uses `(millihertz + 499) / 1000`, matching the
  source rather than replacing it with a more conventional formula.
- Action triggers compare the complete normalized pressed-keysym set, not a
  subset.

Two action-binding discrepancies need a compatibility decision. The current C++
handler returns “block input” when the XML's `no_block` flag is set, and its
`triggered` call appears to pass `trigger_flags` where the XML declares
`time_lo`. The pure state model preserves the first observable behavior. The
Rust wire implementation follows the XML argument order for the event. Before a
replacement release, a trace against the Steam client should determine whether
legacy event-word ordering needs a selectable quirk.

## Implementation order

1. **Completed nested frontend:** core Wayland, nested GLES output, input,
   presentation, dma-buf, Gamescope protocols, and Xwayland/XWM lifecycle.
2. **Completed Steam integration:** multiple and dynamically managed Xwayland
   instances, Steam launch environment, root/window properties, app discovery,
   focus backchannels, overlay/input/content policy, and Remote Play video.
3. **Completed first hardware path:** isolated atomic KMS/GBM/EGL, direct
   scanout and plane promotion with composition fallback, explicit sync,
   hotplug/DPMS, page-flip scheduling, VRR, and dynamic mode control.
4. **Production renderer/backends:** port the exact Vulkan shader/color path,
   then HDR, leasing, multi-output, SDL, OpenVR, headless, and the remaining
   vblank predictor refinements.
5. **Feature completion:** PipeWire producer, screenshots, color and LUTs, mura,
   ReShade, scripting, input emulation, and helper applications.
6. **Replacement qualification:** differential traces, fault injection, soak
   tests, vkms coverage, nested desktop coverage, and real AMD/Intel/NVIDIA and
   Steam Deck hardware matrices.

## Testing strategy

Every ported policy should first become a pure unit test with vectors taken from
the C++ implementation. Each protocol then gets an in-process client/server test
covering bind versions, initial events, valid requests, destructor cleanup,
unknown enum values, and client disconnect during asynchronous work.

At subsystem boundaries, run C++ and Rust with the same scripted client and
normalize traces into comparable records:

- Wayland requests/events and resource lifetimes;
- X11 properties, focus decisions, and selected layer IDs;
- commit metadata, acquire/release points, latch deadline, and feedback time;
- composition plan, formats, modifiers, color spaces, and direct-scanout reason;
- DRM atomic properties or nested Wayland commits.

Rendering tests should compare deterministic off-screen images with per-format
tolerances. DRM tests should start under vkms; direct scanout, modifiers, VRR,
HDR metadata, and timing still require real-hardware suites. The atomic path is
compiled and its pure mailbox/selection/EDID/scheduling policy is tested, but
this checkout's active desktop owns the available physical connector, so no
destructive live modeset was attempted. A Rust port is
not feature-compatible merely because it builds or displays a frame; it is
compatible when these externally visible traces and failure paths agree.
