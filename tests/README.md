# Tests

Unit tests live alongside the code they exercise (look for `#[cfg(test)]`
blocks inside `src/`). Run them with:

```sh
cargo test --lib --all-features
```

There are **no checked-in integration samples** — multi-hundred-MB MKVs don't
belong in a git repo. If you want to iterate against a real master/donor pair
for splice or correlation tuning, cut a small matching sample once from any
two releases you already have locally:

```sh
ffmpeg -i master.mkv -ss 00:16:00.320 -to 00:16:51.080 -map 0 -c copy master.sample.mkv
ffmpeg -i donor.mkv  -ss 00:15:57.320 -to 00:16:47.080 -map 0 -c copy donor.sample.mkv
```

Pick start/end timestamps that you know straddle a boundary in the offset map
(an ad break, a scene cut where the donor was re-edited, etc.). Then run
`dubsync` against the samples with reduced correlation parameters so the
~50-second clip still produces enough anchors:

```sh
cargo run --release --bin dubsync -- \
  --master-file master.sample.mkv --donor-file donor.sample.mkv \
  --master-anchor-track 1 --donor-anchor-track 1 --donor-dub-tracks 2 \
  --output-file out.sample.mkv --keep-temp \
  --correlation-window-s 10 --max-drift-s 5
```

A full pipeline run on a 50-second sample takes ~1.6 s vs ~30 s on a 50-minute
episode — useful for tuning splice flags or auditing the `--keep-temp`
diagnostic dumps without burning a coffee break per iteration.
