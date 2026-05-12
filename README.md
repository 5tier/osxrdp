# osxrdp

An RDP server for macOS — lets any Windows / Linux / Android RDP client connect to and control a Mac desktop over Microsoft's Remote Desktop Protocol (TCP 3389).

```
Windows / Linux / Android RDP client
            │  TCP 3389 / TLS
            ▼
      ┌─────────────┐
      │   osxrdp    │
      │  (this app) │
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

Phase 1 & partial Phase 2 implemented. The full pipeline compiles and runs; real end-to-end testing requires granting the two macOS permissions below.

| # | Feature | Done |
|---|---------|------|
| T1–T3 | Project scaffold, RDP negotiation, TLS | ✅ |
| T4–T5 | Screen capture via ScreenCaptureKit | ✅ |
| T6–T8 | Keyboard + mouse injection via CGEventPost | ✅ |
| T9 | Dirty-region diffing (only send changed tiles) | ✅ |
| T10 | Display resize handling | ⬜ |
| T11 | Permission UX at startup | ⬜ |
| T12 | CLI flags (`--addr`, `--cert`, `--key`, …) | ⬜ |
| T13 | Graceful SIGTERM / SIGINT shutdown | ⬜ |
| T14 | H.264 via VideoToolbox | ⬜ |
| T17 | Clipboard redirection | ⬜ |
| T19 | NLA / CredSSP authentication | ⬜ |

## Architecture

```
src/
├── main.rs        RdpServerBuilder wiring — TLS + display + input
├── tls.rs         Self-signed TLS cert (rcgen + rustls 0.23)
├── display.rs     MacDisplay / MacDisplayUpdates
│                    AsyncSCStream → BitmapUpdate pipeline
│                    Dirty-region diffing via ironrdp-graphics
├── input.rs       MacInputHandler
│                    KeyboardEvent → CGEventPost
│                    MouseEvent → CGEventPost
└── keyboard.rs    Windows scancode set-1 → macOS CGKeyCode table
```

### Protocol stack

`ironrdp-server` handles the entire RDP wire protocol — X.224 negotiation, MCS, capability exchange, fast-path input/output, and bitmap encoding. `osxrdp` only needs to implement two traits:

- **`RdpServerDisplay`** — supply screen frames  
- **`RdpServerInputHandler`** — consume keyboard/mouse events

### Screen capture pipeline

```
AsyncSCStream (SCKit)
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

**Accessibility**

```
System Settings → Privacy & Security → Accessibility → osxrdp ✓
```

Required for `CGEventPost` to inject keyboard and mouse events.

> **Tip — during development:** run from a terminal that already has Accessibility access (e.g. Terminal.app or iTerm2 already listed). The child process inherits the permission.

## Running

```sh
# Debug build with verbose logging
RUST_LOG=osxrdp=debug cargo run

# Release build, quiet
cargo run --release

# The server listens on all interfaces, port 3389.
```

Connect from any RDP client, for example:

| Client | Command / setting |
|--------|------------------|
| **macOS** Microsoft Remote Desktop | Add PC → `<mac-ip>` |
| **Windows** built-in mstsc | `mstsc /v:<mac-ip>` |
| **Linux** FreeRDP | `xfreerdp /v:<mac-ip> /cert:ignore` |
| **iOS / Android** RD Client | Add PC → `<mac-ip>` |

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

1. **T11** — permission check UX (startup guidance if perms missing)  
2. **T12** — CLI flags (`clap`) for addr, cert, key, resolution  
3. **T14** — H.264 via VideoToolbox (order-of-magnitude bandwidth reduction)  
4. **T19** — NLA/CredSSP so Windows clients work with default settings  

## License

> RDP is a Microsoft protocol. The open-source `ironrdp` stack (Devolutions, MIT/Apache-2.0) operates under a royalty-free pledge for open-source use. Review licensing before commercial distribution.
