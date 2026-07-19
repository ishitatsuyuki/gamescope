# Debugging the Rust compositor on a live Gamescope session

This guide records the techniques that were most useful while bringing up the
Rust DRM, Steam, Xwayland, input, direct-scanout, and presentation paths. The
main lesson is to observe every boundary independently. A correct Gamescope
root property does not prove X core focus, a focused X window does not prove a
Wayland keyboard enter was delivered, and a visible frame does not prove that
KMS is scanning out the client buffer.

The examples assume the Rust compositor is running on DRM with Steam on
Xwayland display `:2`. Substitute the display, process, window, connector, and
DRM card from the session being investigated.

## Capture the identity of the session first

Record the compositor PID, exact executable, build ID, Xwayland display, game
window, output mode, and relevant root properties before changing anything.
This prevents a common failure mode: testing an old process after rebuilding
the executable on disk.

```sh
pgrep -a gamescope-rs
pgrep -a Xwayland

pid=$(pgrep -n gamescope-rs)
readelf -n "/proc/$pid/exe" | rg 'Build ID'
readelf -n target/release/gamescope-rs | rg 'Build ID'
```

The two build IDs must match. `/proc/$pid/exe` continues to identify the image
actually mapped by the process even when the file on disk has been replaced.

Discover the X11 hierarchy and keep the selected game window ID handy:

```sh
DISPLAY=:2 xwininfo -root -tree
DISPLAY=:2 xrandr --current
DISPLAY=:2 xprop -root \
  GAMESCOPE_FOCUSED_WINDOW \
  GAMESCOPE_FOCUSED_APP \
  GAMESCOPE_INPUT_COUNTER \
  GAMESCOPE_DIRECT_SCANOUT_STATUS \
  GAMESCOPE_COMPOSITE_FORCE
```

`xwininfo` was particularly useful because it exposed the Xwayland reparenting
hierarchy as well as the game geometry. It made resolution errors such as a
2560x1440 game being configured into only one quarter of the output obvious.

## Treat each boundary as a separate hypothesis

For a Steam X11 game, the important boundaries are:

| Boundary | Question | Useful evidence |
|---|---|---|
| Process | Is the intended build running? | PID, `/proc/PID/exe`, ELF build ID |
| Steam policy | Which app and window did Gamescope select? | `GAMESCOPE_FOCUSED_*`, window-role atoms |
| XWM | Is the window mapped, activated, and geometrically correct? | `xwininfo`, `xprop`, `_NET_WM_STATE` |
| X core input | Which window does the X server actually focus? | `XGetInputFocus` |
| Wayland input | Did the Xwayland `wl_surface` receive seat enter/events? | targeted `xev` plus compositor input counter |
| Render policy | Which base, override, and overlay surfaces were selected? | focused window, overlay atoms, targeted tracing |
| DRM | Was the client promoted to the KMS primary plane? | scanout status and DRM atomic state |
| Presentation | When was the frame latched and when did it flip? | submit/latch trace, page-flip metadata, presentation feedback |

Do not collapse adjacent rows into one conclusion. The hardest bugs in this
bring-up occurred when one layer looked correct while the next layer was not.

## X11 and Wayland focus debugging

There are at least five relevant notions of focus:

1. Gamescope's selected input window.
2. EWMH activation, including `_NET_WM_STATE_FOCUSED`.
3. X server core input focus.
4. Smithay seat focus and the associated Xwayland `wl_surface`.
5. The application's ICCCM input model.

Inspect the policy and ICCCM state together:

```sh
window=0x4c00001

DISPLAY=:2 xprop -root \
  GAMESCOPE_FOCUSED_WINDOW GAMESCOPE_FOCUSED_APP
DISPLAY=:2 xprop -id "$window" \
  WM_NAME WM_CLASS WM_HINTS WM_PROTOCOLS _NET_WM_STATE
```

Do not use `_NET_ACTIVE_WINDOW` or `_NET_WM_STATE_FOCUSED` as a substitute for
X core focus. Query `XGetInputFocus` directly. A minimal Xlib probe is enough:

```c
#include <X11/Xlib.h>
#include <stdio.h>

int main(void) {
    Display *display = XOpenDisplay(NULL);
    Window focus = None;
    int revert_to = 0;
    if (!display)
        return 1;
    XGetInputFocus(display, &focus, &revert_to);
    printf("focus=0x%lx revert_to=%d\n", focus, revert_to);
    XCloseDisplay(display);
    return 0;
}
```

Compile it with `cc x-focus.c -lX11 -o x-focus` and run it with the target
`DISPLAY`.

The EZ2ON input bug demonstrated why this matters. Gamescope's selected-window
property and EWMH state both named the game, but X core focus eventually became
`None`. The window used the globally-active ICCCM model:

- `WM_HINTS` had `input=False`;
- `WM_PROTOCOLS` contained `WM_TAKE_FOCUS`.

Smithay correctly sent the ICCCM message, but the game did not restore core
focus itself. Upstream Gamescope deliberately calls `XSetInputFocus` for its
chosen keyboard window even in this mode. Comparing against that behavior
identified the missing compatibility step.

### Separate input arrival from input delivery

Use the root input counter to prove that physical input reached Gamescope, then
listen on the selected X window to prove that Xwayland delivered keyboard
events:

```sh
DISPLAY=:2 xprop -root GAMESCOPE_INPUT_COUNTER
timeout 20s env DISPLAY=:2 xev -id "$window" -event keyboard
DISPLAY=:2 xprop -root GAMESCOPE_INPUT_COUNTER
```

Interpret the result carefully:

- A rising input counter with no `xev` key event means the problem is after
  libinput dispatch, or that only pointer/controller activity occurred.
- `xev` proves X delivery only for the event types selected by the probe.
- XTEST injection begins inside the X server and therefore bypasses the real
  libinput -> Wayland -> Xwayland path.
- A uinput probe exercises more of the real path, but only if `/dev/uinput`,
  udev classification, seat assignment, and permissions are all valid. A stale
  device node that fails with `ENODEV` is not evidence about compositor input.

A click that makes keyboard input start working is useful evidence, but it also
changes the state under investigation. It commonly points to a missing initial
seat enter or an Xwayland surface-association race. Reproduce once without a
click before adding logging or probes.

In this bring-up, X11 mapping and focus could precede xwayland-shell's
association of the corresponding `wl_surface`. Re-entering the same logical
target after association was necessary because focus equality otherwise
suppressed the Wayland keyboard enter. Late `WM_HINTS` and `WM_PROTOCOLS`
updates required the same scrutiny.

## Resolution, scaling, and coordinate debugging

When pixels are blurry or the game occupies only part of the display, record
all four sizes instead of assuming “the resolution” is a single value:

1. Physical KMS connector mode.
2. Gamescope's logical Wayland output mode.
3. Xwayland root size.
4. Selected game window and buffer size.

Useful X11 probes are:

```sh
DISPLAY=:2 xrandr --current
DISPLAY=:2 xwininfo -root
DISPLAY=:2 xwininfo -id "$window"
```

For DRM, inspect the connector mode and CRTC state through the compositor log,
`modetest`, or debugfs when permitted. A mismatch between the X root/window and
the physical mode explains both scaling blur and the classic “top-left quarter”
symptom. If rendering and input disagree, verify that pointer coordinates use
the inverse of the exact render transform, including centered offsets,
overscan, magnification, and global scale.

## Proving direct scanout instead of inferring it

The Rust compositor publishes the primary-plane decision through
`GAMESCOPE_DIRECT_SCANOUT_STATUS`:

| Value | Meaning |
|---:|---|
| 0 | Client buffer is active on the primary plane |
| 1 | Direct scanout is unavailable |
| 2 | Client format/modifier is unsupported |
| 3 | The atomic scanout test failed |
| 4 | Composition is required by policy |

Read the status with composition force disabled:

```sh
DISPLAY=:2 xprop -root \
  GAMESCOPE_DIRECT_SCANOUT_STATUS GAMESCOPE_COMPOSITE_FORCE
```

The strongest live test was a controlled A/B transition. Force composition,
require status 4, restore the property, and require status 0. Always restore
the original value, including on interruption:

```sh
restore_composition() {
  DISPLAY=:2 xprop -root -f GAMESCOPE_COMPOSITE_FORCE 32c \
    -set GAMESCOPE_COMPOSITE_FORCE 0
}
trap restore_composition EXIT INT TERM

DISPLAY=:2 xprop -root -f GAMESCOPE_COMPOSITE_FORCE 32c \
  -set GAMESCOPE_COMPOSITE_FORCE 1
DISPLAY=:2 xprop -root GAMESCOPE_DIRECT_SCANOUT_STATUS

restore_composition
DISPLAY=:2 xprop -root GAMESCOPE_DIRECT_SCANOUT_STATUS
trap - EXIT INT TERM
```

When debugfs is accessible, corroborate the status with KMS state:

```sh
find /sys/kernel/debug/dri -maxdepth 2 -name state \
  -exec rg -n -A16 '^plane\[[0-9]+\]:.*type=PRI' {} +
```

The primary-plane framebuffer should switch from a client-buffer pool to the
compositor swapchain while composition is forced, then switch back. A GBM
framebuffer marked as a same-GPU import is not evidence against direct scanout;
the decisive evidence is which framebuffer the atomic primary plane uses.

## Diagnosing black frames and overlays

An alive game process with Steam UI still renderable narrows the fault toward
surface selection, buffer import, composition, or scanout rather than process
launch. Start with:

```sh
pgrep -a -f 'steam_app_|EZ2ON'
tail -n 200 "$HOME/steam-APPID.log"
DISPLAY=:2 xwininfo -root -tree
```

Then compare these states:

- game only;
- Steam overlay visible;
- overlay hidden again;
- composition forced;
- direct scanout restored.

If the game becomes black only while an overlay is present, inspect both alpha
and opaque regions. Xwayland may expose a full-window opaque-region hint even
when the Steam overlay buffer contains transparent pixels. If the renderer
trusts that hint, damage/occlusion culling can discard the game underneath and
leave the clear color visible. The Rust fix wraps blended overlay elements and
returns no opaque region for them. A small fake render-element test proved the
occlusion behavior without requiring Steam.

Also filter zero-opacity overlay windows and force composition whenever a
visible blended layer exists. Overlay presence makes primary-plane promotion
of the base game alone invalid even if the game buffer itself is scanoutable.

## Presentation timing and frame pacing

Frame pacing needs separate timestamps for:

1. client commit/present ID;
2. compositor latch or DRM submission;
3. page-flip completion;
4. predicted next presentation time sent to the WSI client.

Use one clock domain. DRM monotonic page-flip metadata should be compared with
`CLOCK_MONOTONIC`, not with process-relative `Instant` values presented as if
they were protocol timestamps.

Unity with VSync was a useful stress case. Delaying Gamescope's present-wait
event until page-flip completion adds an entire frame of backpressure. The
useful release point is the latest latch/submission deadline, with feedback
predicting the next display time. Page-flip completion remains valuable for
updating the vblank model and measuring misses, but it should not unnecessarily
hold the client's present queue.

Avoid millisecond polling as a scheduling mechanism. It both consumes CPU and
adds phase-dependent jitter. The Wayland/input, XWM, and DRM threads should
exchange bounded/coalesced mailboxes and wake through event-loop sources. This
keeps X11 property round trips and atomic commits away from latency-critical
input dispatch while still waking immediately for new work.

## Finding X11 property feedback loops

Every `XChangeProperty`, even one that writes the same value, can generate a
`PropertyNotify` for clients selecting property changes. A compositor that both
watches and publishes a root atom can therefore turn a harmless-looking state
assignment into a busy loop.

Triangulate these loops from three directions:

1. Per-process and per-thread CPU usage (`top -H`, `pidstat`).
2. Syscall rate and dominant calls (`strace -f -c -p PID`).
3. Repeating X11 property notifications (`xev -root -event property`) and atom
   names (`xlsatoms`, `xprop`).

In the VRR case, this correlation was unusually clear: Steam consumed roughly
94% CPU and handled about 35,000 syscalls per second while its main thread
received a continuous stream of notifications for the three VRR atoms. The
steamwebhelper activity was downstream noise; identifying the repeating atom
IDs localized the source loop first.

Then trace both the writer and reader in source. The VRR loop crossed four
boundaries:

```text
DRM OutputChanged
  -> publish hardware VRR atoms
  -> observe our own VRR_ENABLED PropertyNotify as a Steam request
  -> apply the same DRM request
  -> emit OutputChanged again
```

Upstream Gamescope documents the intended ownership:

- `GAMESCOPE_VRR_ENABLED` is a Steam/user preference and request.
- `GAMESCOPE_VRR_CAPABLE` is hardware capability feedback.
- `GAMESCOPE_VRR_FEEDBACK` reports actual VRR usage.

The robust fix closed the loop at three points:

- hardware feedback has no API parameter capable of rewriting
  `GAMESCOPE_VRR_ENABLED`;
- capability and usage feedback are cached and written only when changed;
- DRM ignores an unchanged requested preference and emits `OutputChanged` only
  when its hardware-reported state changes.

This is a general rule for compatibility atoms: assign one owner and one
direction to each property, cache publications at the X11 write boundary, and
make request application idempotent. Deduplication at only one layer is less
safe because hotplug, initialization, or another writer can reactivate a loop.

## Use upstream Gamescope as an executable specification

Protocol XML defines wire layout, but non-obvious focus, timing, and property
behavior often lives in the C++ policy. Search the upstream path before
“cleaning up” behavior that looks redundant:

```sh
rg -n 'XSetInputFocus|WM_TAKE_FOCUS' src/steamcompmgr.cpp
rg -n 'update_vrr_atoms|gamescopeVRREnabled' src/steamcompmgr.cpp
rg -n 'PresentWait|presentation|vblank' src layer
```

Two apparently redundant upstream choices were essential here:

- explicit `XSetInputFocus` coexists with ICCCM `WM_TAKE_FOCUS`;
- `GAMESCOPE_VRR_ENABLED` is intentionally not synchronized to DRM usage.

Comments around such code are compatibility requirements until differential
testing proves otherwise.

## VT and input-session debugging

VT switching crosses raw key handling, compositor policy, libseat/session
state, DRM pause/resume, and focus restoration. Verify each stage:

- the input counter changes for the chord;
- the compositor recognizes Ctrl+Alt+F1..F12 before forwarding it;
- `change_vt` is requested;
- DRM/session pause and resume events arrive;
- X and Wayland focus are reasserted after returning.

Keep the chord-to-VT conversion as a pure unit test. Live tests should include
switching away and back while a game is running, because resume ordering can
expose races that never occur at startup.

## A practical live-debugging loop

1. Record PID, build ID, display, selected window, modes, and root atoms.
2. Reproduce once without clicking, opening an overlay, or switching VTs unless
   that action is the trigger.
3. Decide which boundary first disagrees with expectation.
4. Add the smallest probe at that boundary; avoid global logging on timing-
   sensitive paths.
5. Change one state at a time and record before/after values.
6. Restore every mutable diagnostic property after an experiment.
7. Compare surprising behavior with the equivalent C++ Gamescope path.
8. Turn the root cause into a pure state test or a socket-level protocol test.
9. Run formatting, workspace tests, and an optimized build:

   ```sh
   cargo fmt --all
   cargo test --workspace
   cargo build --workspace --release
   ```

10. Restart and verify the new process build ID before accepting a live result.

The most useful probes produced compact, falsifiable facts: “physical input
arrived but no X key was delivered,” “policy selected the game but X core focus
is None,” “forcing composition changes scanout status from 0 to 4,” or “three
root atoms are rewritten every few milliseconds.” Those facts localize a bug
far more effectively than a large undifferentiated trace.
