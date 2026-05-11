# Assets

Single source of truth: **`logo.png`** — 2048×2048 RGBA PNG with transparent
corners around the squircle. Replace this one file and rebuild to update the
icon on every platform.

At build time the master is fanned out automatically:

- **Windows** — `build.rs` reads `logo.png`, Lanczos3-downsamples to
  16/20/24/32/40/48/64/96/128/256, packs them into `dubsync.ico` via the
  `image` + `ico` crates, and winresource embeds the result into both
  `dubsync.exe` and `dubsync-gui.exe`. The installer (`dubsync.iss`) picks up
  the same `dubsync.ico` via `SetupIconFile`.
- **macOS** — `installer/macos/build-app.sh` reads `logo.png`, generates a
  canonical `.iconset` via `sips` (16/32/64/128/256/512 plus @2x variants),
  and runs `iconutil -c icns` to produce `dubsync.icns`, copied into
  `Dubsync.app/Contents/Resources/`. No external tools beyond what ships
  with macOS.

`dubsync.ico` and `dubsync.icns` are generated artifacts — both are
gitignored, only `logo.png` is checked in. The pipeline degrades cleanly:
if `logo.png` is missing, the build still succeeds and falls back to the
platform default icon.
