# osxrdp

An RDP server for macOS — lets any Windows / Linux / Android RDP client connect to and control a Mac desktop over Microsoft's Remote Desktop Protocol (TCP 3389).

```
Windows / Linux / Android RDP client
            │  TCP 3389 / TLS
            ▼
      ┌─────────────┐
      │   osxrdp    │
      │  (this app)  │
      └──────┬──────┘
             │
    ┌────────┴────────┐
    │                 │
ScreenCaptureKit   CGEventPost
  (screen pixels)  (keyboard + mouse)
    │                 │
    └────────┬────────┘
             │
       macOS Desktop
```

## Status

Phase 1–3 (T1–T11, T14–T15) implemented. End-to-end tested with FreeRDP; real screen capture requires granting the Screen Recording macOS permission.

| # | Feature | Done |
|---|---------|------|
| T1–T3 | Project scaffold, RDP negotiation, TLS | ✅ |
| T4–T5 | Screen capture via ScreenCaptureKit | ✅ |
| T6–T8 | Keyboard + mouse injection via CGEventPost | ✅ |
| T9 | Dirty-region diffing (only send changed tiles) | ✅ |
| T10 | Display resize handling | ✅ |
| T11 | Permission UX at startup | ✅ |
| T12 | CLI flags (`--addr`, `--cert`, `--key`, …) | 🔶 env vars only |
| T13 | Graceful SIGTERM / SIGINT shutdown | ⬜ |
| T14 | H.264 via VideoToolbox | ✅ |
| T15 | Frame rate cap via SCKit `minimum_frame_interval` | ✅ |
| T16 | Multi-monitor support | ⬜ |
| T17 | Clipboard redirection | ⬜ |
| T18 | Audio redirection | ⬜ |
| T19 | NLA / CredSSP authentication | ⬜ |
| T20 | launchd plist (background agent) | ⬜ |

## Architecture

```
src/
├── main.rs        RdpServerBuilder wiring — TLS + display + input + H.264 mode
├── tls.rs         Self-signed TLS cert (rcgen + rustls 0.23)
├── display.rs     MacDisplay / MacDisplayUpdates
│                    AsyncSCStream → BitmapUpdate  (BGRA fallback)
│                    AsyncSCStream → VtH264Encoder → Avc420Update  (H.264 mode)
│                    Dirty-region diffing via ironrdp-graphics
├── h264.rs        VideoToolbox H.264 encoder (raw macOS FFI)
│                    CVPixelBuffer (NV12) → AVCC-format H.264 NAL units
├── input.rs       MacInputHandler
│                    KeyboardEvent → CGEventPost
│                    MouseEvent → CGEventPost
└── keyboard.rs    Windows scancode set-1 → macOS CGKeyCode table
```

### Vendored libraries

The `vendor/` directory contains locally patched copies of upstream `ironrdp` crates. These are used instead of the crates.io versions via `[patch.crates-io]` in `Cargo.toml`.

| Crate | Upstream | Why vendored? |
|-------|-----------|---------------|
| `ironrdp-acceptor` | `ironrdp-acceptor 0.8` | Domain-agnostic credential comparison (RDP clients send arbitrary domain values; the upstream version rejects anything that isn't an exact match) |
| `ironrdp-server` | `ironrdp-server 0.10` | H.264 AVC420 support: adds `DisplayUpdate::Avc420` variant, `Avc420Update` type, `GfxServer` / `SharedGfxServer` RDPGFX DVC channel processor, and `RdpServer::set_gfx_server` wiring |

When upgrading these crates, re-apply the local patches or merge upstream changes into the vendored copies. Key files modified in `vendor/ironrdp-server/src/`:

```
display.rs    — Avc420Update struct, DisplayUpdate::Avc420 variant
encoder/mod.rs — skip Avc420 in surface-command encoder
gfx.rs        — new file: RDPGFX DVC channel (capabilities, surface, WireToSurface1/AVC420)
lib.rs        — pub mod gfx, pub use gfx::*
server.rs     — gfx_server field, set_gfx_server(), attach_channels, client_loop Avc420 routing
```

### Protocol stack

`ironrdp-server` handles the entire RDP wire protocol — X.224 negotiation, MCS, capability exchange, fast-path input/output, and bitmap encoding. `osxrdp` only needs to implement two traits:

- **`RdpServerDisplay`** — supply screen frames
- **`RdpServerInputHandler`** — consume keyboard/mouse events

For H.264 mode, frames also flow through the **RDPGFX** dynamic virtual channel (`Microsoft::Windows::RDS::Graphics`), which sends `WireToSurface1(AVC420)` PDUs carrying VideoToolbox-encoded H.264 NAL units.

### Screen capture pipeline — BGRA mode (default before T14)

```
AsyncSCStream (SCKit, BGRA)
    │  CMSampleBuffer @ ≤30 fps
    ▼
CVPixelBuffer lock → &[u8] (kCVPixelFormatType_32BGRA)
    │
    ├─ First frame / resolution change → full BitmapUpdate
    │
    └─ Subsequent frames
         │
         ▼
         find_different_rects_sub::<4>()   (64×64 tile diff)
         │  Vec<Rect>  (only changed tiles)
         ▼
         BitmapUpdate::sub()  per dirty rect
         │  Bytes slice (zero-copy)
         ▼
         VecDeque<BitmapUpdate>  (drained one per poll)
```

On a static desktop this reduces per-frame bandwidth by ~90 %.

### Screen capture pipeline — H.264 mode (T14)

```
AsyncSCStream (SCKit, YCbCr_420v / NV12)
    │  CMSampleBuffer @ target 30 fps
    ▼
CVPixelBuffer → VtH264Encoder  (VideoToolbox hardware encode)
    │  AVCC-format H.264 NAL units
    ▼
Avc420Update { data, width, height, is_keyframe }
    │
    ▼
GfxServer (RDPGFX DVC channel)
    │  StartFrame → WireToSurface1(AVC420) → EndFrame
    ▼
DrdynvcServer → RDP client
```

H.264 mode is enabled by default (OSXRDP_H264=1). It provides order-of-magnitude bandwidth reduction vs. raw BGRA bitmaps.

### Input injection

`CGEventPost(kCGHIDEventTap, ...)` injects events at the HID level, so they reach whichever application has focus. Keyboard events use scancode → CGKeyCode translation; unicode input uses `CGEvent::set_string_from_utf16_unchecked` for correct IME handling.

## Requirements

| Requirement | Version |
|-------------|---------|
| macOS | 12.3+ (ScreenCaptureKit) |
| Rust toolchain | 1.75+ |
| Xcode Command Line Tools | any recent (full Xcode not required) |
| Swift | ships with CLT / Xcode |

## Dev environment setup

### 1. Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 2. Install Xcode Command Line Tools (if not already present)

```sh
xcode-select --install
```

> Full Xcode is not required — Command Line Tools is sufficient. The `screencapturekit` crate compiles a Swift bridge at build time; `build.rs` ensures the Swift runtime rpath is set correctly for both CLT and full Xcode installs.

### 3. Clone and build

```sh
git clone <repo-url> osxrdp
cd osxrdp
cargo build          # debug build
cargo build --release  # optimised build
```

### 4. Grant macOS permissions

osxrdp needs two permissions. Grant them **before** running, or the server will start but deliver blank frames / ignored input.

**Screen Recording**

```
System Settings → Privacy & Security → Screen Recording → osxrdp ✓
```

Or trigger the dialog by running the binary once — macOS will prompt automatically on the first `SCShareableContent` call.

**Accessibility / Input Injection**

`CGEventPost` (keyboard + mouse injection) works without a separate permission on recent macOS versions. If input is not responding, try running osxrdp from a terminal that is already listed under Privacy & Security → Accessibility (Terminal.app, iTerm2, etc.).

## Running

```sh
# Debug build with verbose logging
RUST_LOG=osxrdp=debug cargo run

# Release build, quiet
cargo run --release

# Disable H.264 (fall back to BGRA bitmap mode)
OSXRDP_H264=0 cargo run

# The server listens on all interfaces, port 3389.
```

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OSXRDP_ADDR` | `0.0.0.0:3389` | Listen address and port |
| `OSXRDP_USER` | `admin` | RDP username |
| `OSXRDP_PASSWORD` | `admin` | RDP password |
| `OSXRDP_H264` | `1` | Enable H.264 VideoToolbox encoding (`0` = BGRA fallback) |

### Credentials

RDP clients must authenticate. The default credentials are **`admin` / `admin`**. Override with environment variables:

```sh
OSXRDP_USER=alice OSXRDP_PASSWORD=secret cargo run
```

> **Windows / NLA:** Windows `mstsc` defaults to requiring NLA (CredSSP). Until T19 is implemented, connect with FreeRDP (`/cert:ignore`) or configure mstsc to allow TLS-only: Options → Advanced → uncheck "Always ask for credentials" and set Authentication to "No Authentication".

Connect from any RDP client, for example:

| Client | Command / setting |
|--------|------------------|
| **macOS** Microsoft Remote Desktop | Add PC → `<mac-ip>`, Username: `admin`, Password: `admin` |
| **Windows** built-in mstsc | `mstsc /v:<mac-ip>` (disable NLA — see note above) |
| **Linux** FreeRDP | `xfreerdp /v:<mac-ip> /cert:ignore /u:admin /p:admin` |
| **iOS / Android** RD Client | Add PC → `<mac-ip>`, Username: `admin`, Password: `admin` |

> **TLS certificate:** osxrdp generates a self-signed cert on every start. Clients will show an untrusted-certificate warning — accept it. For a persistent cert, replace `tls::build_acceptor()` with one that loads a cert from disk (T12 will add `--cert`/`--key` flags).

> **NLA not yet implemented (T19):** if your Windows client forces NLA, disable it: `mstsc` → Options → Advanced → "Connect and don't warn me" or connect to an older-security RDP endpoint. Alternatively, on the client run `mstsc /v:<ip> /admin`.

## Logging

Log level is controlled by `RUST_LOG`:

```sh
RUST_LOG=osxrdp=trace cargo run   # everything including per-frame diffs
RUST_LOG=osxrdp=info  cargo run   # connection events only
RUST_LOG=off          cargo run   # silent
```

## Contributing / roadmap

See [`tasks.md`](tasks.md) for the full task list. Highest-value next items:

1. **T13** — graceful SIGTERM / SIGINT shutdown
2. **T16** — multi-monitor support
3. **T19** — NLA/CredSSP so Windows clients work with default settings

## License

> RDP is a Microsoft protocol. The open-source `ironrdp` stack (Devolutions, MIT/Apache-2.0) operates under a royalty-free pledge for open-source use. Review licensing before commercial distribution.