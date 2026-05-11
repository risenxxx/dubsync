# Contributing

Thanks for the interest. Bug reports, feature ideas, and PRs are all welcome.

## Setup

- Rust 1.75 or newer (`rustup install stable` is fine).
- `ffmpeg` + `ffprobe` on `PATH` — required at runtime for every code path.
- `rubberband` on `PATH` — only needed if you touch the `--smooth-gaps` splice path.

Build:

```sh
cargo build --release                         # full build, includes the GUI
cargo build --release --no-default-features   # CLI only (no eframe/rfd/etc.)
```

## Pre-PR checklist

Run all three locally before pushing — CI runs the same set and will reject the PR otherwise:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib --all-features
```

## Fast iteration on the splice pipeline

The full pipeline takes ~30 s on hour-long real-world inputs. For tuning splice / correlation flags, cut a small pair of matching samples once with `ffmpeg -ss/-to ... -c copy` and run against the samples with reduced correlation parameters — a 50-second clip then completes in ~1.6 s:

```sh
cargo run --release --bin dubsync -- \
  --master-file ./master.sample.mkv --donor-file ./donor.sample.mkv \
  --master-anchor-track 1 --donor-anchor-track 1 --donor-dub-tracks 2 \
  --output-file ./out.sample.mkv \
  --keep-temp \
  --correlation-window-s 10 --max-drift-s 5
```

`--keep-temp` leaves the workspace dir behind with diagnostic JSON dumps (offset map, correlation traces, master silences, etc.) — useful when investigating a misplaced splice.

## Filing issues / PRs

- Bugs: include the exact CLI invocation, master + donor track layout (`ffprobe -show_streams`), and the relevant log lines (set `RUST_LOG=debug` for verbose output).
- PRs: keep changes focused; a single PR per concern. Reference the issue number in the description if applicable.
