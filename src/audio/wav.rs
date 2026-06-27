use crate::error::{DubsyncError, Result};
use hound::{SampleFormat, WavReader, WavSpec};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::Path;

/// Read a mono f32 WAV (the format we always extract anchors as).
/// Returns the samples plus the sample rate.
pub fn read_mono_f32(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;

    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => reader.samples::<f32>().collect::<hound::Result<Vec<_>>>()?,
        (SampleFormat::Int, bits) => {
            // Defensive path — we always ask ffmpeg for f32le, but if a user pipes in
            // something else we still cope. Scale to [-1, 1].
            let scale = 1.0_f32 / ((1i64 << (bits - 1)) as f32);
            reader
                .samples::<i32>()
                .collect::<hound::Result<Vec<_>>>()?
                .into_iter()
                .map(|s| s as f32 * scale)
                .collect()
        }
        other => {
            return Err(crate::error::DubsyncError::Wav(hound::Error::Unsupported)).map_err(|e| {
                tracing::error!(?other, "unexpected WAV sample format");
                e
            })
        }
    };

    let channels = spec.channels as usize;
    let mono: Vec<f32> = if channels == 1 {
        samples
    } else {
        // Defensive path. Anchors are always extracted with ffmpeg `-ac 1` upstream,
        // so this branch should never fire in production. If a future caller pipes a
        // multichannel WAV through here, we keep only the first channel rather than
        // averaging — averaging would silently zero out content that is symmetric and
        // out-of-phase across channels (e.g. inverted-stereo masters), which would
        // corrupt the GCC-PHAT correlation.
        tracing::debug!(
            channels,
            "read_mono_f32 received multichannel input — using first channel only"
        );
        let frames = samples.len() / channels;
        (0..frames).map(|f| samples[f * channels]).collect()
    };

    Ok((mono, sample_rate))
}

/// Decoded interleaved PCM exactly as it lives on disk.
pub struct Pcm {
    pub samples: Vec<f32>,
    pub spec: WavSpec,
}

impl Pcm {
    pub fn channels(&self) -> usize {
        self.spec.channels as usize
    }
    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels()
    }
    pub fn duration_s(&self) -> f64 {
        self.frames() as f64 / self.sample_rate() as f64
    }
}

/// Essentials parsed from a RIFF/RF64 WAVE header, with the data-chunk length
/// already resolved against the file's real size (see [`parse_wav_header`]).
struct WavHeader {
    spec: WavSpec,
    /// Byte offset of the first sample (start of the `data` chunk payload).
    data_offset: u64,
    /// Number of *usable* PCM bytes, floored to a whole frame.
    data_len: u64,
}

impl WavHeader {
    fn frame_bytes(&self) -> u64 {
        self.spec.channels as u64 * (self.spec.bits_per_sample as u64 / 8)
    }
}

/// Parse a RIFF/RF64 WAVE header by hand and resolve the true data-chunk length.
///
/// Why not `hound`: ffmpeg's `wav` muxer stores chunk sizes in 32-bit fields, so a
/// `pcm_f32le` track whose data exceeds 4 GiB (a native 5.1 / 7.1 feature is well
/// over an hour) is written with a placeholder `0xFFFFFFFF` data-size and ffmpeg
/// itself warns "output file will be broken". `hound` then rejects the file
/// ("data chunk length is not a multiple of sample size", because `0xFFFFFFFF % 4
/// == 3`) — and even a *corrected* header could not help, since hound carries the
/// data length in a `u32` and so cannot address a >4 GiB chunk at all.
///
/// We therefore ignore an unreliable declared size and fall back to the actual
/// bytes on disk (`file_len - data_offset`), then floor to a whole frame. This
/// transparently handles the 4 GiB overflow placeholder, RF64's `ds64` 64-bit
/// sizes, *and* a truncated/interrupted extraction (we read every complete frame
/// that made it to disk instead of erroring out).
fn parse_wav_header(file: &mut File, file_len: u64) -> Result<WavHeader> {
    let fmt_err = |m: &'static str| DubsyncError::Wav(hound::Error::FormatError(m));

    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    // RIFF = canonical 32-bit; RF64/BW64 = 64-bit sizes carried in a `ds64` chunk.
    // RIFX (big-endian) is not produced by ffmpeg and not supported here.
    let is_rf64 = &magic == b"RF64" || &magic == b"BW64";
    if &magic != b"RIFF" && !is_rf64 {
        return Err(fmt_err("not a RIFF/RF64 WAVE file"));
    }
    let mut buf4 = [0u8; 4];
    file.read_exact(&mut buf4)?; // overall RIFF size — unreliable for >4 GiB, ignored
    file.read_exact(&mut buf4)?;
    if &buf4 != b"WAVE" {
        return Err(fmt_err("missing WAVE form type"));
    }

    let mut channels: Option<u16> = None;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut is_float = false;
    let mut ds64_data_size: Option<u64> = None;
    let mut data: Option<(u64, u64)> = None; // (offset, declared_len)

    // Walk the chunk list. Each chunk is a 4-byte id + 4-byte LE size + payload,
    // padded to an even byte boundary.
    let mut pos = 12u64;
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        let mut id = [0u8; 4];
        if file.read_exact(&mut id).is_err() {
            break;
        }
        file.read_exact(&mut buf4)?;
        let declared = u32::from_le_bytes(buf4) as u64;
        let body = pos + 8;
        match &id {
            b"fmt " => {
                let want = declared.min(40) as usize;
                if want < 16 {
                    return Err(fmt_err("fmt chunk too small"));
                }
                let mut fb = vec![0u8; want];
                file.read_exact(&mut fb)?;
                let fmt_tag = u16::from_le_bytes([fb[0], fb[1]]);
                channels = Some(u16::from_le_bytes([fb[2], fb[3]]));
                sample_rate = u32::from_le_bytes([fb[4], fb[5], fb[6], fb[7]]);
                bits = u16::from_le_bytes([fb[14], fb[15]]);
                is_float = match fmt_tag {
                    0x0003 => true,  // WAVE_FORMAT_IEEE_FLOAT
                    0x0001 => false, // WAVE_FORMAT_PCM (integer)
                    // WAVE_FORMAT_EXTENSIBLE: the real format lives in the SubFormat
                    // GUID, whose first two bytes mirror the classic format tag.
                    0xFFFE => want >= 26 && u16::from_le_bytes([fb[24], fb[25]]) == 0x0003,
                    _ => false,
                };
            }
            b"ds64" if declared >= 16 => {
                // RF64 64-bit sizes: riffSize(8), dataSize(8), sampleCount(8), …
                let mut db = [0u8; 16];
                file.read_exact(&mut db)?;
                ds64_data_size = Some(u64::from_le_bytes(db[8..16].try_into().unwrap()));
            }
            b"data" => {
                data = Some((body, declared));
            }
            _ => {}
        }
        // Advance to the next chunk (word-aligned). A bogus/placeholder size (the
        // `0xFFFFFFFF` data size, or anything that runs past EOF) means there is no
        // trustworthy next chunk — stop here. `data` is the final chunk in every
        // file we read, so we have already recorded it by this point.
        let padded = declared + (declared & 1);
        match body.checked_add(padded) {
            Some(next) if next > pos && next <= file_len => pos = next,
            _ => break,
        }
    }

    let channels = channels.ok_or_else(|| fmt_err("missing fmt chunk"))?;
    if channels == 0 {
        return Err(fmt_err("zero-channel WAV"));
    }
    if bits == 0 || bits % 8 != 0 {
        return Err(fmt_err("unsupported bit depth"));
    }
    let (data_offset, declared_len) = data.ok_or_else(|| fmt_err("missing data chunk"))?;

    let frame_bytes = channels as u64 * (bits as u64 / 8);
    let avail = file_len.saturating_sub(data_offset);
    // Prefer the most trustworthy length: RF64 64-bit size, else a sane 32-bit
    // declared size, else the bytes actually present on disk. Then floor to a whole
    // frame so the sample count is exact even on a truncated file.
    let raw_len = match ds64_data_size {
        Some(d) if d <= avail => d,
        _ if declared_len != 0 && declared_len != 0xFFFF_FFFF && declared_len <= avail => {
            declared_len
        }
        _ => avail,
    };
    let data_len = raw_len - (raw_len % frame_bytes);

    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: bits,
        sample_format: if is_float {
            SampleFormat::Float
        } else {
            SampleFormat::Int
        },
    };
    Ok(WavHeader {
        spec,
        data_offset,
        data_len,
    })
}

/// Read a multichannel f32 WAV with channels left interleaved.
///
/// Uses [`parse_wav_header`] (not `hound`) so that dubs whose `pcm_f32le` data
/// exceeds the 4 GiB WAV size-field limit — and files truncated by an interrupted
/// extraction — are read by their real on-disk length rather than rejected.
pub fn read_interleaved_f32(path: &Path) -> Result<Pcm> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let header = parse_wav_header(&mut file, file_len)?;
    if header.spec.sample_format != SampleFormat::Float || header.spec.bits_per_sample != 32 {
        // Be strict here — we control upstream extraction (always pcm_f32le).
        return Err(DubsyncError::Wav(hound::Error::Unsupported));
    }

    let n_samples = (header.data_len / 4) as usize; // 4 bytes per f32 sample
    let mut samples = vec![0.0_f32; n_samples];

    file.seek(SeekFrom::Start(header.data_offset))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    // Stream the payload in fixed blocks, converting LE bytes straight into the
    // preallocated buffer. Reading into a separate `Vec<u8>` first would double the
    // peak memory for a multi-GB track; this keeps it to one large allocation.
    let mut block = [0u8; 1 << 16]; // 64 KiB, a multiple of 4
    let mut filled = 0usize;
    while filled < n_samples {
        let want_samples = (block.len() / 4).min(n_samples - filled);
        let want_bytes = want_samples * 4;
        reader.read_exact(&mut block[..want_bytes])?;
        for (i, c) in block[..want_bytes].chunks_exact(4).enumerate() {
            samples[filled + i] = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        filled += want_samples;
    }

    Ok(Pcm {
        samples,
        spec: header.spec,
    })
}

/// Write an interleaved f32 WAV.
///
/// Why not `hound`: its `WavWriter` accumulates the data length in a `u32` byte
/// counter, so a synced output exceeding 4 GiB (a full feature at native 5.1/7.1 —
/// the same payloads that overflow on extraction) corrupts the size on finalize and
/// fails. A canonical WAV cannot represent a >4 GiB size at all, so for those we
/// emit **RF64** (EBU Tech 3306): an `RF64` magic plus a `ds64` chunk carrying the
/// real 64-bit sizes, with the legacy 32-bit fields left as the `0xFFFFFFFF`
/// sentinel. ffmpeg (which muxes the synced WAV) and [`read_interleaved_f32`] both
/// read RF64. Sub-4 GiB files keep the ordinary canonical RIFF layout.
pub fn write_interleaved_f32(path: &Path, samples: &[f32], spec: &WavSpec) -> Result<()> {
    // RF64 once the canonical 32-bit size fields can no longer hold the totals.
    let data_bytes = samples.len() as u64 * 4;
    let use_rf64 = data_bytes + 128 > u32::MAX as u64;
    write_f32_with_format(path, samples, spec, use_rf64)
}

/// Inner writer with an explicit container choice so tests can exercise the RF64
/// path without materialising a >4 GiB file.
fn write_f32_with_format(
    path: &Path,
    samples: &[f32],
    spec: &WavSpec,
    use_rf64: bool,
) -> Result<()> {
    use std::io::Write;

    let channels = spec.channels as u64;
    let sample_rate = spec.sample_rate as u64;
    let bits = 32u64; // we only ever write f32le
    let bytes_per_sample = bits / 8;
    let block_align = channels * bytes_per_sample;
    let byte_rate = sample_rate * block_align;
    let data_bytes = samples.len() as u64 * bytes_per_sample;
    let frames = (samples.len() as u64).checked_div(channels).unwrap_or(0);

    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);

    // ── fmt chunk body (16 bytes, WAVE_FORMAT_IEEE_FLOAT) ──
    let mut fmt = Vec::with_capacity(16);
    fmt.extend_from_slice(&3u16.to_le_bytes()); // wFormatTag = IEEE float
    fmt.extend_from_slice(&(channels as u16).to_le_bytes());
    fmt.extend_from_slice(&(sample_rate as u32).to_le_bytes());
    fmt.extend_from_slice(&(byte_rate as u32).to_le_bytes());
    fmt.extend_from_slice(&(block_align as u16).to_le_bytes());
    fmt.extend_from_slice(&(bits as u16).to_le_bytes());

    if use_rf64 {
        w.write_all(b"RF64")?;
        w.write_all(&0xFFFF_FFFFu32.to_le_bytes())?; // riff size in ds64
        w.write_all(b"WAVE")?;
        // ds64: riffSize, dataSize, sampleCount (per channel), tableLength.
        let riff_size = 80u64 + data_bytes - 8; // whole file minus the RF64+size fields
        w.write_all(b"ds64")?;
        w.write_all(&28u32.to_le_bytes())?;
        w.write_all(&riff_size.to_le_bytes())?;
        w.write_all(&data_bytes.to_le_bytes())?;
        w.write_all(&frames.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // table length
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&fmt)?;
        w.write_all(b"data")?;
        w.write_all(&0xFFFF_FFFFu32.to_le_bytes())?; // data size in ds64
    } else {
        // Canonical RIFF: RIFF + (fmt 8+16) + (fact 8+4) + (data 8+N).
        let riff_size = 4 + 24 + 12 + 8 + data_bytes;
        w.write_all(b"RIFF")?;
        w.write_all(&(riff_size as u32).to_le_bytes())?;
        w.write_all(b"WAVE")?;
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&fmt)?;
        w.write_all(b"fact")?;
        w.write_all(&4u32.to_le_bytes())?;
        w.write_all(&(frames as u32).to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&(data_bytes as u32).to_le_bytes())?;
    }

    // Stream the payload in fixed blocks so we never materialise a multi-GB byte
    // buffer alongside the sample Vec.
    let mut block: Vec<u8> = Vec::with_capacity(1 << 16);
    for chunk in samples.chunks(1 << 14) {
        block.clear();
        for &s in chunk {
            block.extend_from_slice(&s.to_le_bytes());
        }
        w.write_all(&block)?;
    }
    w.flush()?;
    Ok(())
}

/// Header-only metadata for a WAV file: frames, sample rate, channel count. Used by
/// the fps-stretch phase to compute expected output sizes without reading the entire
/// file into memory (a 1-hour episode at 48 kHz stereo is ~1.4 GB on disk).
pub struct WavInfo {
    pub frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Read just the WAV header — does not load samples.
///
/// Shares [`parse_wav_header`] with [`read_interleaved_f32`] so the fps-stretch
/// phase computes frame counts from the file's real size and is immune to the same
/// 4 GiB / truncated-data issues (hound's `WavReader::open` would reject those).
pub fn probe_frames(path: &Path) -> Result<WavInfo> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let header = parse_wav_header(&mut file, file_len)?;
    Ok(WavInfo {
        frames: header.data_len / header.frame_bytes(),
        sample_rate: header.spec.sample_rate,
        channels: header.spec.channels,
    })
}

#[cfg(test)]
pub(crate) fn float_spec(channels: u16, sample_rate: u32) -> WavSpec {
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Offset of the 4-byte little-endian data-chunk size field in `bytes`.
    fn data_size_field_offset(bytes: &[u8]) -> usize {
        let data = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("data chunk present");
        data + 4
    }

    /// Round-trips a stereo f32 WAV, then overwrites its data-size field with the
    /// `0xFFFFFFFF` placeholder ffmpeg writes once the real size overflows 32 bits.
    /// The reader must recover the full sample set from the file's real length.
    #[test]
    fn reads_past_overflow_placeholder_data_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dub.wav");
        let spec = float_spec(2, 48_000);
        // 1000 interleaved stereo frames = 2000 samples.
        let samples: Vec<f32> = (0..2000).map(|i| (i as f32) * 1e-4).collect();
        write_interleaved_f32(&path, &samples, &spec).unwrap();

        // Corrupt the data-size field in place: 0xFFFFFFFF, exactly as ffmpeg's wav
        // muxer leaves it for a >4 GiB payload (and 0xFFFFFFFF % 4 == 3, which is the
        // value that makes hound reject the file).
        let mut bytes = std::fs::read(&path).unwrap();
        let off = data_size_field_offset(&bytes);
        bytes[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let pcm = read_interleaved_f32(&path).unwrap();
        assert_eq!(pcm.channels(), 2);
        assert_eq!(pcm.sample_rate(), 48_000);
        assert_eq!(pcm.frames(), 1000);
        assert_eq!(pcm.samples.len(), 2000);
        assert!((pcm.samples[1999] - 1999.0 * 1e-4).abs() < 1e-9);

        // probe_frames must agree without loading samples.
        let info = probe_frames(&path).unwrap();
        assert_eq!(info.frames, 1000);
        assert_eq!(info.channels, 2);
    }

    /// A file truncated mid-frame (e.g. an interrupted extraction) must read every
    /// *complete* frame on disk rather than erroring — the length is floored to a
    /// whole frame.
    #[test]
    fn truncated_file_floors_to_whole_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dub.wav");
        let spec = float_spec(2, 48_000);
        let samples: Vec<f32> = (0..2000).map(|i| i as f32).collect();
        write_interleaved_f32(&path, &samples, &spec).unwrap();

        // Drop the last 5 bytes: kills one stereo frame (8 bytes) partially, so the
        // last whole frame ends one frame earlier → 999 frames survive.
        let mut bytes = std::fs::read(&path).unwrap();
        // Also blank the (now-wrong) declared size so the file-length path is taken.
        let off = data_size_field_offset(&bytes);
        bytes[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.truncate(bytes.len() - 5);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let pcm = read_interleaved_f32(&path).unwrap();
        assert_eq!(pcm.frames(), 999);
        assert_eq!(pcm.samples.len(), 1998);
    }

    /// The RF64 writer path (forced here so we don't need a >4 GiB file) must
    /// round-trip through the reader, which resolves the data length from the
    /// `ds64` chunk rather than the `0xFFFFFFFF` legacy field.
    #[test]
    fn rf64_round_trips_through_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rf64.wav");
        let spec = float_spec(6, 48_000); // 5.1, like the real dubs
        let samples: Vec<f32> = (0..600).map(|i| (i as f32) * 1e-3).collect(); // 100 frames
        write_f32_with_format(&path, &samples, &spec, true).unwrap();

        // Header must be RF64, not RIFF.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RF64");
        assert!(bytes.windows(4).any(|w| w == b"ds64"));

        let pcm = read_interleaved_f32(&path).unwrap();
        assert_eq!(pcm.channels(), 6);
        assert_eq!(pcm.frames(), 100);
        assert_eq!(pcm.samples.len(), 600);
        assert!((pcm.samples[599] - 599.0 * 1e-3).abs() < 1e-6);

        let info = probe_frames(&path).unwrap();
        assert_eq!(info.frames, 100);
        assert_eq!(info.channels, 6);
    }

    /// A well-formed file with a valid declared size still reads correctly through
    /// the new path (no regression for the common case).
    #[test]
    fn reads_well_formed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.wav");
        let spec = float_spec(1, 16_000);
        let samples: Vec<f32> = (0..500).map(|i| (i as f32) * 0.001).collect();
        write_interleaved_f32(&path, &samples, &spec).unwrap();

        let pcm = read_interleaved_f32(&path).unwrap();
        assert_eq!(pcm.channels(), 1);
        assert_eq!(pcm.frames(), 500);
        assert!((pcm.samples[499] - 0.499).abs() < 1e-9);
    }
}
