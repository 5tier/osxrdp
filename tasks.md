# osxrdp — Implementation Tasks

## Phase 1: Core Server (wire the pipeline end-to-end)

- [x] **T1** Scaffold Rust project, module skeleton, cargo check passes
- [x] **T2** Restructure: replace custom TCP/RDP stack with `ironrdp-server` builder
- [x] **T3** TLS: self-signed cert via `rcgen` + `rustls 0.23`
- [x] **T4** Display: `MacDisplay` / `MacDisplayUpdates` implementing `RdpServerDisplay`
- [x] **T5** Screen capture: `AsyncSCStream` (screencapturekit 2.x) → `BitmapUpdate`
- [x] **T6** Input handler: `MacInputHandler` implementing `RdpServerInputHandler`
- [x] **T7** Keyboard: Windows scancode (u8 from ironrdp) → macOS `CGKeyCode` via `core-graphics`
- [x] **T8** Mouse: `CGEvent` move / click / scroll injection via `core-graphics`

## Phase 2: Correctness & Usability

- [x] **T9**  Dirty-region diffing: only send changed tiles (`find_different_rects_sub` + `BitmapUpdate::sub`)
- [x] **T10** Display resize: handle `DisplayUpdate::Resize` from `request_layout`
- [x] **T11** Permission UX: check Screen Recording + Accessibility at startup, print actionable instructions if missing
- [~] **T12** CLI flags: `--addr`, `--cert`, `--key`, `--width`, `--height` via `clap` (env vars `OSXRDP_ADDR`, `OSXRDP_USER`, `OSXRDP_PASSWORD` working; clap flags TBD)
- [ ] **T13** Graceful shutdown: catch SIGTERM/SIGINT, call `RdpServer` quit event

## Phase 3: Performance

- [x] **T14** H.264 encoding via `VideoToolbox` (YCbCr 420 path through screencapturekit)
- [x] **T15** Frame rate cap: honour SCKit `minimum_frame_interval` to target 30 fps
- [ ] **T16** Multi-monitor: enumerate all displays, let client choose via CLI flag

## Phase 4: Features

- [ ] **T17** Clipboard redirection (`RDPCLIP` virtual channel via `ironrdp-cliprdr`)
- [ ] **T18** Audio redirection (`RDPSND` via `ironrdp-rdpsnd` + CoreAudio tap)
- [ ] **T19** NLA / CredSSP authentication (`RdpServerSecurity::Hybrid` + `sspi-rs`)
- [ ] **T20** launchd plist: run as a background agent on login
