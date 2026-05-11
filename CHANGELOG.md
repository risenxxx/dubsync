# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Each release corresponds to a tag of the form `vX.Y.Z` on `main`; full
per-platform archives are attached to the
[GitHub Releases page](https://github.com/risenxxx/dubsync/releases).

## [Unreleased]

## [0.9.0] - 2026-05-11

### Added
- macOS (Apple Silicon) release packaging: signed-ad-hoc `.app` + `.dmg`,
  Homebrew-bundled `ffmpeg` / `rubberband` rewritten to `@executable_path/...`.
- Native Windows decorations + rounded corners (DWM) for the GUI binary.

### Changed
- Default dub output codec is now **E-AC3** (was FLAC). Matches the typical
  donor codec and plays back natively on TVs / AVRs over HDMI.
- Muxer's per-stream interleave-delta gate disabled to keep audio packets
  flowing on long stretches without video keyframes.

## [0.8.x and earlier]

See the
[GitHub Releases page](https://github.com/risenxxx/dubsync/releases) for
per-version notes prior to 0.9.0.
