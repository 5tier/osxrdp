# AGENTS.md

## Build & Test

```sh
cargo build              # debug
cargo build --release    # release (LTO, 1 codegen unit)
cargo test --release     # tests (only in CI; no src/ tests, only vendor crate tests)
```

No clippy, rustfmt, or other linters are configured. CI runs `cargo build --release` + `cargo test --release` only.

## Hard Rules

**Always restore original display resolution.** Three layers enforce this — never remove any:

1. `DisplayModeSwitcher::restore()` on Drop (clean exit)
2. `/tmp/osxrdp_pending_restore` file (crash recovery, read on next startup)
3. `CGConfigureOption::ConfigureForSession` (session-scoped, reverts on logout)

## macOS Bugs You Must Work Around

1. **SCKit crashes after any display mode change** — use `CGDisplayCreateImage` (`CaptureSource::CGDisplay`) instead of ScreenCaptureKit after a mode switch.

2. **`CGDisplayMode::all_display_modes` and `CGDisplaySetDisplayMode` crash after a mode change** — ALL mode enumeration and switching runs in a **child process** (`osxrdp --display-mode-switch`). Parent only reads safe getters (`pixels_wide/pixels_high`).

If you need to add new CoreGraphics display APIs, check whether they crash post-mode-change and route them through the child process if so.

## Vendored Crates

`vendor/` contains locally patched ironrdp crates, wired via `[patch.crates-io]` in `Cargo.toml`:

| Crate | Why vendored |
|-------|-------------|
| `ironrdp-acceptor` | Domain-agnostic credential comparison |
| `ironrdp-server` | H.264 AVC420 support + RDPGFX DVC channel |
| `ironrdp-pdu-0.7.0` | GFX capability version handling |

When upgrading, re-apply local patches. Key modified files in `vendor/ironrdp-server/src/`: `display.rs`, `encoder/mod.rs`, `gfx.rs`, `lib.rs`, `server.rs`.

## H.264 Is Disabled by Default

`OSXRDP_H264=0` (default). The GFX server doesn't handle dynamic resize properly (size mismatch between display stream and GFX surface). Use BGRA mode until fixed. Enable with `OSXRDP_H264=1`.

## build.rs Gotcha

Do **not** add the CLT swift-5.5 path (`/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx`) to the rpath. It contains a real `.dylib` that conflicts with the system dyld cache version at `/usr/lib/swift`, causing "class implemented in both" ObjC warnings. The `build.rs` already handles this correctly.

## Source Map

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point, CLI dispatch for display mode helpers, server wiring |
| `src/display.rs` | Screen capture stream, `CaptureSource` enum (SCKit vs CGDisplay), `request_layout()` triggers mode switch |
| `src/frame_pipeline.rs` | `BgraFramePipeline` — letterbox, dirty-region detection, sub-region update queue for BGRA frames |
| `src/display_mode.rs` | `DisplayModeSwitcher`, child process mode APIs, crash recovery |
| `src/cg_capture.rs` | `CGDisplayCreateImage` capture, BGRA/H.264 frame production |
| `src/h264.rs` | VideoToolbox H.264 encoder |
| `src/input.rs` | Keyboard/mouse injection via `CGEventPost` |
| `src/keyboard.rs` | Windows scancode → macOS CGKeyCode table |
| `src/clipboard.rs` | Clipboard redirection via `ironrdp-cliprdr` |
| `src/auth.rs` | macOS system account auth via `dscl`/opendirectoryd |
| `src/permissions.rs` | Screen Recording permission check at startup |
| `src/tls.rs` | Self-signed TLS cert (rcgen + rustls) |

## Environment Variables

| Variable | Default | Notes |
|----------|---------|-------|
| `OSXRDP_ADDR` | `0.0.0.0:3389` | Listen address |
| `OSXRDP_H264` | `0` | `1` enables VideoToolbox H.264 |
| `OSXRDP_ASPECT` | `fit` | `fit` = switch display to match client (no black bars), `native` = keep server resolution (black bars if ratio differs) |
| `RUST_LOG` | `osxrdp=info` | `trace` for per-frame diffs |

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues (`github.com/5tier/osxrdp`). See `docs/agents/issue-tracker.md`.

### Triage labels

Default label vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout — one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
