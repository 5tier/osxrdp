# Agents / AI Assistant Notes

## Goal

Make the Mac server's physical display adapt to the RDP client's display
resolution and aspect ratio — **no black bars, no cropping, no
letterboxing.** When a 16:10 client connects to a 21:9 ultrawide Mac, the
Mac switches to a matching 16:10 resolution. When the client disconnects,
the original resolution is restored.

## ⚠️ Hard Rule: Always Restore Original Resolution

**The original display resolution MUST always be restored.** No matter
what — client disconnect, osxrdp crash, SIGSEGV, force-kill, power
failure — the Mac's display must return to its original resolution.

Three layers ensure this:

1. **`DisplayModeSwitcher::restore()` on Drop** — called when osxrdp
   exits cleanly (client disconnect, Ctrl-C). Restores via child process.
2. **`/tmp/osxrdp_pending_restore` file** — written before the mode
   switch, deleted after successful restore. If osxrdp crashes or is
   killed, this file persists and is read on next startup by
   `display_mode::restore_pending()`, which restores the original mode.
3. **`CGConfigureOption::ConfigureForSession`** — the mode change persists
   for the login session but reverts on logout/restart. If both the
   restore and the pending file fail, logging out or restarting the Mac
   will revert to the system default resolution.

**Never remove, weaken, or bypass any of these three layers.**

## The SCKit and CoreGraphics Bugs

**macOS has two bugs that affect display mode switching:**

1. **SCKit crash**: `ScreenCaptureKit` crashes (SIGSEGV in
   `createContentFilterWithDisplay`) after ANY display mode change, even
   from a separate process. SCKit cannot be used after a mode change.

2. **CoreGraphics crash**: `CGDisplayMode::all_display_modes` and
   `CGDisplaySetDisplayMode` crash (SIGSEGV in `__os_lock_handoff_lock_slow`)
   after a display mode change. Both APIs must be called in a child process.

**Solutions:**

- **Screen capture**: When a mode switch has occurred, use
  `CGDisplayCreateImage` instead of SCKit. The `CaptureSource` enum selects
  `Sckit(AsyncSCStream)` or `CGDisplay` at stream creation time.
- **Mode APIs**: ALL display mode APIs (`all_display_modes`,
  `CGDisplaySetDisplayMode`, `display_mode()`) are called in a **child
  process** (`osxrdp --display-mode-switch`). The parent only uses safe
  read-only getters (`CGDisplay::pixels_wide/pixels_high`).

| Operation | Where it runs | Why |
|-----------|--------------|-----|
| Mode enumeration | Child process | `all_display_modes` crashes after mode change |
| Mode switching | Child process | `CGDisplaySetDisplayMode` crashes after mode change |
| Mode restore | Child process | Same crash risk |
| Screen capture (no switch) | Parent process | SCKit works without mode change |
| Screen capture (after switch) | Parent process | CGDisplay capture, SCKit crashes |

CGDisplay capture works by:
1. Polling `CGDisplay::image()` at the target FPS (30fps)
2. Drawing the `CGImage` into a BGRA bitmap context
3. Converting to `Bytes` and feeding through `handle_bgra_data`

## Mode Switching Flow

1. RDP client connects, sends display layout (e.g., 1470×919)
2. `request_layout()` detects aspect mismatch → spawns child process
   `osxrdp --display-mode-switch <id> <w> <h>`
3. Child process enumerates modes, finds best match, switches via
   `CGDisplaySetDisplayMode`, prints `OK <w> <h>` to stdout
4. Parent reads child's stdout to get the actual switched resolution
5. `request_layout()` sets `mode_switch_pending`
6. `updates()` detects `mode_switch_pending` → 500ms delay → uses
   `CaptureSource::CGDisplay` for screen capture
7. On client disconnect → `DisplayModeSwitcher::restore()` → spawns
   `osxrdp --display-mode-restore` child to revert original resolution

## Architecture

- `src/display.rs` — Display update stream. `CaptureSource` enum selects
  SCKit or CGDisplay. `request_layout()` triggers mode switch via child.
- `src/cg_capture.rs` — CoreGraphics screen capture via
  `CGDisplayCreateImage`. Produces BGRA frames for dirty-rect pipeline.
- `src/display_mode.rs` — `DisplayModeSwitcher` struct; parent process
  uses `CGDisplay::pixels_wide/high` only; spawns child for mode APIs;
  `--display-mode-switch` / `--display-mode-restore` CLI helpers;
  crash recovery via `/tmp/osxrdp_pending_restore`
- `src/h264.rs` — VideoToolbox H.264 encoder (only for SCKit capture)
- `src/input.rs` / `src/keyboard.rs` — macOS input event injection
- `src/main.rs` — entry point, CLI arg dispatch for display mode helpers

## Key Permissions

osxrdp requires four macOS permissions:

1. **Screen Recording** — System Settings → Privacy & Security
2. **Accessibility** — System Settings → Privacy & Security
3. **Network Incoming Connections** — firewall prompt on first run
4. **SSH Remote Login** — for remote access to the Mac