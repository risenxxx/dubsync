# Assets

Two icon masters, one per platform — the macOS Dock convention (~10% transparent
inset around the squircle, per Apple HIG) makes the icon look oversized on
Windows, so we keep them separate:

- **`logo.png`** — 2048×2048 RGBA PNG. The Windows master; squircle fills most
  of the canvas. Used as a fallback by the macOS build too, but the result
  looks larger than native macOS apps in the Dock.
- **`logo-macos.png`** (optional) — 2048×2048 RGBA PNG with the visible squircle
  drawn inside a 1648×1648 centred box (≈200 px transparent margin on every
  side, corner radius ≈370 px). Matches the sizing of Apple's default app
  icons. If absent, the macOS build silently falls back to `logo.png`.

At build time the appropriate master is fanned out automatically:

- **Windows** — `build.rs` reads `logo.png`, Lanczos3-downsamples to
  16/20/24/32/40/48/64/96/128/256, packs them into `dubsync.ico` via the
  `image` + `ico` crates, and winresource embeds the result into both
  `dubsync.exe` and `dubsync-gui.exe`. The installer (`dubsync.iss`) picks up
  the same `dubsync.ico` via `SetupIconFile`.
- **macOS** — `installer/macos/build-app.sh` reads `logo-macos.png` if
  present, otherwise `logo.png`. Generates a canonical `.iconset` via `sips`
  (16/32/64/128/256/512 plus @2x variants) and runs `iconutil -c icns` to
  produce `dubsync.icns`, copied into `Dubsync.app/Contents/Resources/`. No
  external tools beyond what ships with macOS.

`dubsync.ico` and `dubsync.icns` are generated artifacts — both are
gitignored, only the masters are checked in. The pipeline degrades cleanly:
if neither master is present, the build still succeeds and falls back to the
platform default icon.

## Screenshot

`screenshot.png` is the macOS hero image embedded in the top-level
[`README.md`](../README.md). Excluded from the crate tarball (see `exclude`
in [`Cargo.toml`](../Cargo.toml)) — not needed to compile, only to render
the GitHub landing page.
