//! The convolution and spatial audio processor node (ADR 19).
//!
//! Provides zero-latency FIR convolution for Head-Related Impulse Responses (HRIR)
//! including 14-channel HeSuVi WAV files, 4-channel True Stereo WAV files, standard
//! stereo and mono impulse responses, alongside a built-in acoustic crossfeed
//! and stereo width spatializer.
//!
//! Runs on the decode thread in the processing chain immediately following the
//! parametric equalizer. Shared parameters are held in [`ConvolverParams`], which
//! uses atomics and an ArcSwap-style update for live, click-free parameter and
//! impulse response swapping.
//!
//! Follows the ADR 19 bypass rule: when disabled or with no impulse response and
//! neutral spatial settings, samples pass bit-exact and internal filter state stays clear.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::chain::Node;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// The maximum supported impulse response length in samples (approx 170 ms at 48 kHz).
/// Typical HRIRs and HeSuVi profiles are 512 to 2048 samples (~10 to 45 ms).
pub const MAX_IR_SAMPLES: usize = 8192;

/// Operating mode for 14-channel HeSuVi impulse responses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ConvolverMode {
    /// Simulates two virtual front stereo speakers placed in front of the listener
    /// using the Front Left (FL) and Front Right (FR) binaural filters.
    #[default]
    VirtualStereo = 0,
    /// Upmixes stereo audio to 7.1 surround sound (FL, FR, FC, SL, SR, BL, BR)
    /// and convolves all 7 surround positions using the full 14-channel HeSuVi HRIR set.
    Surround7_1 = 1,
}

impl ConvolverMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ConvolverMode::Surround7_1,
            _ => ConvolverMode::VirtualStereo,
        }
    }
}

/// Built-in HeSuVi HRIR profiles bundled into the binary for instant out-of-the-box spatialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinHesuviProfile {
    #[default]
    None,
    Atmos,
    Gsx,
    Dtshx,
    CmssGame,
    Sbx67,
    Razer,
    Sonic,
    DolbyHeadphone,
}

impl BuiltinHesuviProfile {
    pub fn all() -> &'static [BuiltinHesuviProfile] {
        &[
            BuiltinHesuviProfile::None,
            BuiltinHesuviProfile::Atmos,
            BuiltinHesuviProfile::Gsx,
            BuiltinHesuviProfile::Dtshx,
            BuiltinHesuviProfile::CmssGame,
            BuiltinHesuviProfile::Sbx67,
            BuiltinHesuviProfile::Razer,
            BuiltinHesuviProfile::Sonic,
            BuiltinHesuviProfile::DolbyHeadphone,
        ]
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            BuiltinHesuviProfile::None => "Built-in Crossfeed",
            BuiltinHesuviProfile::Atmos => "Dolby Atmos",
            BuiltinHesuviProfile::Gsx => "Sennheiser GSX",
            BuiltinHesuviProfile::Dtshx => "DTS Headphone:X",
            BuiltinHesuviProfile::CmssGame => "Creative CMSS-3D",
            BuiltinHesuviProfile::Sbx67 => "Sound BlasterX",
            BuiltinHesuviProfile::Razer => "Razer Surround",
            BuiltinHesuviProfile::Sonic => "Windows Sonic",
            BuiltinHesuviProfile::DolbyHeadphone => "Dolby Headphone",
        }
    }

    pub fn load_ir(&self) -> Option<WavIr> {
        let (name, bytes): (&str, &[u8]) = match self {
            BuiltinHesuviProfile::None => return None,
            BuiltinHesuviProfile::Atmos => ("Dolby Atmos", include_bytes!("../hrir/atmos.wav")),
            BuiltinHesuviProfile::Gsx => ("Sennheiser GSX", include_bytes!("../hrir/gsx.wav")),
            BuiltinHesuviProfile::Dtshx => ("DTS Headphone:X", include_bytes!("../hrir/dtshx.wav")),
            BuiltinHesuviProfile::CmssGame => {
                ("Creative CMSS-3D", include_bytes!("../hrir/cmss_game.wav"))
            }
            BuiltinHesuviProfile::Sbx67 => ("Sound BlasterX", include_bytes!("../hrir/sbx67-.wav")),
            BuiltinHesuviProfile::Razer => ("Razer Surround", include_bytes!("../hrir/razer.wav")),
            BuiltinHesuviProfile::Sonic => ("Windows Sonic", include_bytes!("../hrir/sonic-.wav")),
            BuiltinHesuviProfile::DolbyHeadphone => {
                ("Dolby Headphone", include_bytes!("../hrir/dh+.wav"))
            }
        };
        parse_wav(name, bytes).ok()
    }
}

/// Channel layout detected from an impulse response file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrLayout {
    /// 14-channel HeSuVi HRIR:
    /// - Ch 0: Front Left -> Left ear (FL->L)
    /// - Ch 1: Front Left -> Right ear (FL->R)
    /// - Ch 2: Side Left -> Left ear (SL->L)
    /// - Ch 3: Side Left -> Right ear (SL->R)
    /// - Ch 4: Back Left -> Left ear (BL->L)
    /// - Ch 5: Back Left -> Right ear (BL->R)
    /// - Ch 6: Center -> Left ear (FC->L)
    /// - Ch 7: Front Right -> Right ear (FR->R) [note: right ear first]
    /// - Ch 8: Front Right -> Left ear (FR->L)
    /// - Ch 9: Side Right -> Right ear (SR->R)
    /// - Ch 10: Side Right -> Left ear (SR->L)
    /// - Ch 11: Back Right -> Right ear (BR->R)
    /// - Ch 12: Back Right -> Left ear (BR->L)
    /// - Ch 13: Center -> Right ear (FC->R)
    Hesuvi14,
    /// 4-channel True Stereo:
    /// - Ch 0: Left in -> Left ear (LL)
    /// - Ch 1: Left in -> Right ear (LR)
    /// - Ch 2: Right in -> Left ear (RL)
    /// - Ch 3: Right in -> Right ear (RR)
    TrueStereo4,
    /// Standard 2-channel Stereo:
    /// - Ch 0: Left in -> Left ear
    /// - Ch 1: Right in -> Right ear
    Stereo2,
    /// 1-channel Mono:
    /// - Applied to both channels
    Mono1,
    /// Generic channel count
    Generic(usize),
}

impl IrLayout {
    pub fn from_channels(channels: usize) -> Self {
        match channels {
            14 => IrLayout::Hesuvi14,
            4 => IrLayout::TrueStereo4,
            2 => IrLayout::Stereo2,
            1 => IrLayout::Mono1,
            n => IrLayout::Generic(n),
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            IrLayout::Hesuvi14 => "HeSuVi 14-ch HRIR",
            IrLayout::TrueStereo4 => "True Stereo (4-ch)",
            IrLayout::Stereo2 => "Stereo IR (2-ch)",
            IrLayout::Mono1 => "Mono IR (1-ch)",
            IrLayout::Generic(_) => "Multi-channel IR",
        }
    }
}

/// A parsed impulse response with metadata.
#[derive(Clone, Debug)]
pub struct WavIr {
    pub name: String,
    pub sample_rate: u32,
    pub layout: IrLayout,
    pub channels: Vec<Vec<f32>>,
}

impl WavIr {
    /// Resample this impulse response's channels to the target sample rate if needed.
    pub fn resampled_to(&self, target_rate: u32) -> Self {
        if self.sample_rate == target_rate || self.channels.is_empty() {
            return self.clone();
        }
        let resampled = self
            .channels
            .iter()
            .map(|ch| resample_channel(ch, self.sample_rate, target_rate))
            .collect();
        WavIr {
            name: self.name.clone(),
            sample_rate: target_rate,
            layout: self.layout,
            channels: resampled,
        }
    }
}

/// Parse a RIFF/WAVE file into a [`WavIr`].
/// Supports 16-bit, 24-bit, and 32-bit integer PCM, as well as 32-bit and 64-bit IEEE float,
/// including standard RIFF and WAVE_FORMAT_EXTENSIBLE headers.
pub fn parse_wav(name: &str, data: &[u8]) -> Result<WavIr, String> {
    if data.len() < 12 {
        return Err("WAV data too short".into());
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("Not a valid RIFF/WAVE file".into());
    }

    let mut pos = 12;
    let mut channels: Option<u16> = None;
    let mut sample_rate: Option<u32> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut format_tag: Option<u16> = None;
    let mut sub_format: Option<[u8; 16]> = None;
    let mut raw_data: Option<&[u8]> = None;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(
            data[pos + 4..pos + 8]
                .try_into()
                .map_err(|_| "chunk size slice")?,
        ) as usize;
        pos += 8;

        let end = pos.saturating_add(chunk_size);
        if end > data.len() {
            return Err("WAV chunk truncated".into());
        }

        if chunk_id == b"fmt " {
            if chunk_size < 16 {
                return Err("fmt chunk too small".into());
            }
            let fmt = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap());
            let ch = u16::from_le_bytes(data[pos + 2..pos + 4].try_into().unwrap());
            let sr = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            let bps = u16::from_le_bytes(data[pos + 14..pos + 16].try_into().unwrap());

            format_tag = Some(fmt);
            channels = Some(ch);
            sample_rate = Some(sr);
            bits_per_sample = Some(bps);

            if fmt == 0xFFFE && chunk_size >= 40 {
                let mut guid = [0u8; 16];
                guid.copy_from_slice(&data[pos + 24..pos + 40]);
                sub_format = Some(guid);
            }
        } else if chunk_id == b"data" {
            raw_data = Some(&data[pos..end]);
        }

        // RIFF chunks are word-aligned (padded to even byte boundary)
        pos = end + (chunk_size % 2);
    }

    let (ch_count, rate, bps, fmt) = match (channels, sample_rate, bits_per_sample, format_tag) {
        (Some(c), Some(r), Some(b), Some(f)) if c > 0 && r > 0 => (c as usize, r, b, f),
        _ => return Err("Missing or invalid fmt chunk in WAV".into()),
    };

    let pcm_data = raw_data.ok_or_else(|| "Missing data chunk in WAV".to_string())?;

    // Determine sample encoding
    let is_float = match fmt {
        3 => true,
        0xFFFE => {
            // Check sub-format GUID for IEEE_FLOAT: 00000003-0000-0010-8000-00aa00389b71
            sub_format.is_some_and(|g| g[0] == 3 && g[1] == 0)
        }
        _ => false,
    };

    let bytes_per_sample = (bps as usize) / 8;
    if bytes_per_sample == 0 {
        return Err("Invalid zero bits per sample".into());
    }
    let block_align = ch_count * bytes_per_sample;
    let num_frames = pcm_data.len() / block_align;
    if num_frames == 0 {
        return Err("No audio frames found in WAV data".into());
    }

    // Limit impulse response frames to MAX_IR_SAMPLES to prevent accidental load of full songs
    let frames_to_read = num_frames.min(MAX_IR_SAMPLES);
    let mut channel_data = vec![Vec::with_capacity(frames_to_read); ch_count];

    for frame in 0..frames_to_read {
        let frame_offset = frame * block_align;
        for c in 0..ch_count {
            let sample_offset = frame_offset + c * bytes_per_sample;
            let sample = if is_float {
                match bps {
                    32 => {
                        let bytes: [u8; 4] = pcm_data[sample_offset..sample_offset + 4]
                            .try_into()
                            .unwrap();
                        f32::from_le_bytes(bytes)
                    }
                    64 => {
                        let bytes: [u8; 8] = pcm_data[sample_offset..sample_offset + 8]
                            .try_into()
                            .unwrap();
                        f64::from_le_bytes(bytes) as f32
                    }
                    _ => return Err(format!("Unsupported float bit depth: {bps}")),
                }
            } else {
                match bps {
                    16 => {
                        let bytes: [u8; 2] = pcm_data[sample_offset..sample_offset + 2]
                            .try_into()
                            .unwrap();
                        let v = i16::from_le_bytes(bytes);
                        v as f32 / 32768.0
                    }
                    24 => {
                        let b0 = pcm_data[sample_offset] as i32;
                        let b1 = pcm_data[sample_offset + 1] as i32;
                        let b2 = pcm_data[sample_offset + 2] as i8 as i32; // sign-extended
                        let v = b0 | (b1 << 8) | (b2 << 16);
                        v as f32 / 8388608.0
                    }
                    32 => {
                        let bytes: [u8; 4] = pcm_data[sample_offset..sample_offset + 4]
                            .try_into()
                            .unwrap();
                        let v = i32::from_le_bytes(bytes);
                        v as f32 / 2147483648.0
                    }
                    _ => return Err(format!("Unsupported PCM bit depth: {bps}")),
                }
            };
            channel_data[c].push(sample);
        }
    }

    // Apply a smooth taper (cosine window) at the end of the loaded IR if it was truncated,
    // to prevent any harsh discontinuities.
    if frames_to_read < num_frames {
        let taper_len = 64.min(frames_to_read);
        let start = frames_to_read - taper_len;
        for ch in &mut channel_data {
            for (i, sample) in ch[start..frames_to_read].iter_mut().enumerate() {
                let phase = (i as f32 / taper_len as f32) * std::f32::consts::FRAC_PI_2;
                *sample *= phase.cos();
            }
        }
    }

    // Global normalization across all channels:
    // Scale so peak absolute value across all channels is well-conditioned (e.g. max 0.707).
    // Scaling uniformly preserves all interaural level and delay cues without clipping.
    let global_peak = channel_data
        .iter()
        .flat_map(|ch| ch.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);

    if global_peak > 0.0001 {
        // If peak is too high, normalize to 0.707. If already reasonable, preserve.
        let target_peak = 0.7071f32;
        let scale = if global_peak > 1.0 {
            target_peak / global_peak
        } else if global_peak < 0.1 {
            (0.5 / global_peak).min(4.0)
        } else {
            1.0
        };
        if (scale - 1.0).abs() > 0.01 {
            for ch in &mut channel_data {
                for sample in ch {
                    *sample *= scale;
                }
            }
        }
    }

    // Expand 7-channel HeSuVi compact format to full 14-channel layout.
    // The 7-ch format stores only the to-left-ear impulse responses:
    //   src[0]=FL→L, src[1]=SL→L, src[2]=BL→L, src[3]=FC→L,
    //   src[4]=FR→L, src[5]=SR→L, src[6]=BR→L
    // By head symmetry, the to-right-ear response for a speaker equals
    // the to-left-ear response for its mirror (FL↔FR, SL↔SR, BL↔BR, FC=FC).
    let (final_channels, final_layout) = if ch_count == 7 {
        let mut full = Vec::with_capacity(14);
        // Ch  0: FL→L = src[0]
        full.push(channel_data[0].clone());
        // Ch  1: FL→R = src[4] (= FR→L by symmetry)
        full.push(channel_data[4].clone());
        // Ch  2: SL→L = src[1]
        full.push(channel_data[1].clone());
        // Ch  3: SL→R = src[5] (= SR→L by symmetry)
        full.push(channel_data[5].clone());
        // Ch  4: BL→L = src[2]
        full.push(channel_data[2].clone());
        // Ch  5: BL→R = src[6] (= BR→L by symmetry)
        full.push(channel_data[6].clone());
        // Ch  6: FC→L = src[3]
        full.push(channel_data[3].clone());
        // Ch  7: FR→R = src[0] (= FL→L by symmetry)
        full.push(channel_data[0].clone());
        // Ch  8: FR→L = src[4]
        full.push(channel_data[4].clone());
        // Ch  9: SR→R = src[1] (= SL→L by symmetry)
        full.push(channel_data[1].clone());
        // Ch 10: SR→L = src[5]
        full.push(channel_data[5].clone());
        // Ch 11: BR→R = src[2] (= BL→L by symmetry)
        full.push(channel_data[2].clone());
        // Ch 12: BR→L = src[6]
        full.push(channel_data[6].clone());
        // Ch 13: FC→R = src[3] (= FC→L by symmetry)
        full.push(channel_data[3].clone());
        (full, IrLayout::Hesuvi14)
    } else {
        (channel_data, IrLayout::from_channels(ch_count))
    };

    Ok(WavIr {
        name: name.to_string(),
        sample_rate: rate,
        layout: final_layout,
        channels: final_channels,
    })
}

/// Bandlimited windowed-sinc resampling for a static 1D slice.
fn resample_channel(src: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || src.is_empty() {
        return src.to_vec();
    }
    let out_len = ((src.len() as u64 * dst_rate as u64) / src_rate as u64) as usize;
    if out_len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(out_len);
    let step = src_rate as f64 / dst_rate as f64;
    let ratio = dst_rate as f64 / src_rate as f64;
    let cutoff = (ratio.min(1.0) * 0.95).min(1.0);
    let filter_half = 16isize;

    for i in 0..out_len {
        let src_pos = i as f64 * step;
        let center = src_pos.floor() as isize;
        let frac = src_pos - center as f64;
        let mut sum = 0.0f64;
        let mut weight_sum = 0.0f64;

        for k in -filter_half..=filter_half {
            let idx = center + k;
            if idx >= 0 && (idx as usize) < src.len() {
                let t = (k as f64 - frac) * cutoff;
                let sinc = if t.abs() < 1e-7 {
                    1.0
                } else {
                    (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
                };
                let w_arg = (k as f64 - frac + filter_half as f64) / (2.0 * filter_half as f64);
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * w_arg).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * w_arg).cos();
                let weight = sinc * w * cutoff;
                sum += src[idx as usize] as f64 * weight;
                weight_sum += weight;
            }
        }
        let val = if weight_sum.abs() > 1e-7 {
            (sum / weight_sum) as f32
        } else {
            0.0
        };
        out.push(val);
    }
    out
}

/// Partition block size B for zero-latency hybrid convolution.
pub const PARTITION_LEN: usize = 128;
/// FFT size 2B for Overlap-Save tail convolution.
pub const FFT_LEN: usize = 256;
/// Number of complex frequency bins: FFT_LEN / 2 + 1.
pub const NUM_BINS: usize = 129;

/// Precomputed partitioned impulse response for one filter channel.
#[derive(Clone)]
pub struct PartitionedFilter {
    /// Head impulse response coefficients reversed for forward inner product: length PARTITION_LEN.
    pub head_rev: Vec<f32>,
    /// Precomputed FFT spectra for each tail partition: each has length NUM_BINS.
    pub tail_spectra: Vec<Vec<Complex32>>,
}

impl PartitionedFilter {
    pub fn new(ir: &[f32], r2c: &dyn RealToComplex<f32>) -> Self {
        let b = PARTITION_LEN;
        let head_len = ir.len().min(b);
        let mut head_rev = vec![0.0f32; b];
        for i in 0..head_len {
            head_rev[b - 1 - i] = ir[i];
        }

        let mut tail_spectra = Vec::new();
        if ir.len() > b {
            let tail = &ir[b..];
            let p_tail = (tail.len() + b - 1) / b;
            let mut time_buf = vec![0.0f32; FFT_LEN];
            let mut complex_buf = vec![Complex32::default(); NUM_BINS];

            for p in 0..p_tail {
                let start = p * b;
                let end = (start + b).min(tail.len());
                time_buf.fill(0.0);
                time_buf[..end - start].copy_from_slice(&tail[start..end]);
                r2c.process(&mut time_buf, &mut complex_buf).unwrap();
                tail_spectra.push(complex_buf.clone());
            }
        }

        PartitionedFilter {
            head_rev,
            tail_spectra,
        }
    }
}

/// An input-to-output channel routing tap:
/// specifies which input channel feeds which filter, routing to Left or Right ear with an optional gain scale.
#[derive(Clone, Copy)]
pub struct ChannelRouting {
    pub in_ch: usize,
    pub filter_idx: usize,
    pub to_left: bool,
    pub scale: f32,
}

/// State for one input channel in the partitioned convolver.
#[derive(Clone)]
pub struct InputChannelState {
    pub head_hist: Vec<f32>,
    pub cur_block: Vec<f32>,
    pub prev_block: Vec<f32>,
    pub spectra_history: Vec<Vec<Complex32>>,
}

impl InputChannelState {
    pub fn new(p_tail: usize) -> Self {
        InputChannelState {
            head_hist: vec![0.0; PARTITION_LEN],
            cur_block: vec![0.0; PARTITION_LEN],
            prev_block: vec![0.0; PARTITION_LEN],
            spectra_history: vec![vec![Complex32::default(); NUM_BINS]; p_tail.max(1)],
        }
    }

    pub fn clear(&mut self) {
        self.head_hist.fill(0.0);
        self.cur_block.fill(0.0);
        self.prev_block.fill(0.0);
        for s in &mut self.spectra_history {
            s.fill(Complex32::default());
        }
    }

    #[inline(always)]
    pub fn push_sample(&mut self, x: f32, pos: usize) {
        self.head_hist.copy_within(1..PARTITION_LEN, 0);
        self.head_hist[PARTITION_LEN - 1] = x;
        self.cur_block[pos] = x;
    }
}

#[inline(always)]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// A zero-latency partitioned convolution engine (ADR 19).
///
/// Implements Gardner's zero-latency hybrid algorithm (1995) combined with Uniform
/// Partitioned Overlap-Save (UPOLS):
/// - The head of each impulse response (first 128 samples) runs in the time domain
///   using direct SIMD dot products for exact zero algorithmic latency.
/// - The tail of each impulse response runs in the frequency domain using real FFTs of size 256.
/// - All multi-channel contributions accumulate in the frequency domain before the inverse FFT,
///   requiring only TWO inverse FFTs per block total.
pub struct PartitionedConvolver {
    layout: IrLayout,
    mode: ConvolverMode,
    p_tail: usize,
    block_pos: usize,
    r2c: Arc<dyn RealToComplex<f32>>,
    c2r: Arc<dyn ComplexToReal<f32>>,
    filters: Vec<PartitionedFilter>,
    routings: Vec<ChannelRouting>,
    input_channels: Vec<InputChannelState>,
    tail_block_l: Vec<f32>,
    tail_block_r: Vec<f32>,
    fft_scratch_time: Vec<f32>,
    fft_scratch_freq: Vec<Complex32>,
    accum_freq_l: Vec<Complex32>,
    accum_freq_r: Vec<Complex32>,
    ifft_scratch_time: Vec<f32>,
}

impl PartitionedConvolver {
    pub fn new(
        ir: &WavIr,
        mode: ConvolverMode,
        r2c: Arc<dyn RealToComplex<f32>>,
        c2r: Arc<dyn ComplexToReal<f32>>,
    ) -> Self {
        let b = PARTITION_LEN;
        let filters: Vec<PartitionedFilter> = ir
            .channels
            .iter()
            .map(|ch| PartitionedFilter::new(ch, r2c.as_ref()))
            .collect();

        let p_tail = filters
            .iter()
            .map(|f| f.tail_spectra.len())
            .max()
            .unwrap_or(0);

        let layout = ir.layout;
        let (num_inputs, routings) = match layout {
            IrLayout::Hesuvi14 if filters.len() >= 14 => match mode {
                ConvolverMode::VirtualStereo => (
                    2,
                    vec![
                        ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 0, filter_idx: 1, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 1, filter_idx: 8, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 1, filter_idx: 7, to_left: false, scale: 1.0 },
                    ],
                ),
                ConvolverMode::Surround7_1 => (
                    7,
                    vec![
                        ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 0, filter_idx: 1, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 1, filter_idx: 8, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 1, filter_idx: 7, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 2, filter_idx: 6, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 2, filter_idx: 13, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 3, filter_idx: 2, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 3, filter_idx: 3, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 4, filter_idx: 10, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 4, filter_idx: 9, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 5, filter_idx: 4, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 5, filter_idx: 5, to_left: false, scale: 1.0 },
                        ChannelRouting { in_ch: 6, filter_idx: 12, to_left: true, scale: 1.0 },
                        ChannelRouting { in_ch: 6, filter_idx: 11, to_left: false, scale: 1.0 },
                    ],
                ),
            },
            IrLayout::TrueStereo4 if filters.len() >= 4 => (
                2,
                vec![
                    ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                    ChannelRouting { in_ch: 0, filter_idx: 1, to_left: false, scale: 1.0 },
                    ChannelRouting { in_ch: 1, filter_idx: 2, to_left: true, scale: 1.0 },
                    ChannelRouting { in_ch: 1, filter_idx: 3, to_left: false, scale: 1.0 },
                ],
            ),
            IrLayout::Stereo2 if filters.len() >= 2 => (
                2,
                vec![
                    ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                    ChannelRouting { in_ch: 1, filter_idx: 1, to_left: false, scale: 1.0 },
                ],
            ),
            IrLayout::Mono1 if !filters.is_empty() => (
                2,
                vec![
                    ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                    ChannelRouting { in_ch: 1, filter_idx: 0, to_left: false, scale: 1.0 },
                ],
            ),
            _ => (
                2,
                vec![
                    ChannelRouting { in_ch: 0, filter_idx: 0, to_left: true, scale: 1.0 },
                    ChannelRouting {
                        in_ch: 1,
                        filter_idx: if filters.len() > 1 { 1 } else { 0 },
                        to_left: false,
                        scale: 1.0,
                    },
                ],
            ),
        };

        let input_channels = vec![InputChannelState::new(p_tail); num_inputs];

        PartitionedConvolver {
            layout,
            mode,
            p_tail,
            block_pos: 0,
            r2c,
            c2r,
            filters,
            routings,
            input_channels,
            tail_block_l: vec![0.0; b],
            tail_block_r: vec![0.0; b],
            fft_scratch_time: vec![0.0; FFT_LEN],
            fft_scratch_freq: vec![Complex32::default(); NUM_BINS],
            accum_freq_l: vec![Complex32::default(); NUM_BINS],
            accum_freq_r: vec![Complex32::default(); NUM_BINS],
            ifft_scratch_time: vec![0.0; FFT_LEN],
        }
    }

    pub fn clear(&mut self) {
        self.block_pos = 0;
        for ch in &mut self.input_channels {
            ch.clear();
        }
        self.tail_block_l.fill(0.0);
        self.tail_block_r.fill(0.0);
    }

    /// Process one stereo frame (in_l, in_r) through the impulse response convolver with zero latency.
    #[inline(always)]
    pub fn process_frame(&mut self, in_l: f32, in_r: f32, channel_gains: &[f32; 7]) -> (f32, f32) {
        let pos = self.block_pos;

        // Push samples to input channels according to mode, scaled by per-channel gains
        match (self.layout, self.mode) {
            (IrLayout::Hesuvi14, ConvolverMode::Surround7_1) => {
                // HeSuVi official stereo upmix matrix
                let fl = 0.5 * in_l * channel_gains[0];
                let fr = 0.5 * in_r * channel_gains[1];
                let fc = 0.2 * (in_l + in_r) * channel_gains[2];
                let sl = (0.45 * in_l - 0.25 * in_r) * channel_gains[3];
                let sr = (-0.25 * in_l + 0.45 * in_r) * channel_gains[4];
                let bl = (0.3 * in_l - 0.2 * in_r) * channel_gains[5];
                let br = (-0.2 * in_l + 0.3 * in_r) * channel_gains[6];

                self.input_channels[0].push_sample(fl, pos);
                self.input_channels[1].push_sample(fr, pos);
                self.input_channels[2].push_sample(fc, pos);
                self.input_channels[3].push_sample(sl, pos);
                self.input_channels[4].push_sample(sr, pos);
                self.input_channels[5].push_sample(bl, pos);
                self.input_channels[6].push_sample(br, pos);
            }
            _ => {
                self.input_channels[0].push_sample(in_l * channel_gains[0], pos);
                self.input_channels[1].push_sample(in_r * channel_gains[1], pos);
            }
        }
        let mut out_l = self.tail_block_l[pos];
        let mut out_r = self.tail_block_r[pos];

        for r in &self.routings {
            let hist = &self.input_channels[r.in_ch].head_hist;
            let head = &self.filters[r.filter_idx].head_rev;
            let head_val = dot_product(head, hist) * r.scale;
            if r.to_left {
                out_l += head_val;
            } else {
                out_r += head_val;
            }
        }

        // Advance block position; run tail UPOLS block when full
        self.block_pos += 1;
        if self.block_pos == PARTITION_LEN {
            self.block_pos = 0;
            self.process_block_tail();
        }

        (out_l, out_r)
    }

    fn process_block_tail(&mut self) {
        let b = PARTITION_LEN;
        let fft_size = FFT_LEN;
        let p_tail = self.p_tail;
        if p_tail == 0 {
            return;
        }

        // 1. Forward FFT for each input channel
        for in_ch in &mut self.input_channels {
            self.fft_scratch_time[..b].copy_from_slice(&in_ch.prev_block);
            self.fft_scratch_time[b..].copy_from_slice(&in_ch.cur_block);
            in_ch.prev_block.copy_from_slice(&in_ch.cur_block);

            self.r2c
                .process(&mut self.fft_scratch_time, &mut self.fft_scratch_freq)
                .unwrap();

            in_ch.spectra_history.rotate_right(1);
            in_ch.spectra_history[0].copy_from_slice(&self.fft_scratch_freq);
        }

        // 2. Frequency-domain accumulation
        self.accum_freq_l.fill(Complex32::default());
        self.accum_freq_r.fill(Complex32::default());

        for r in &self.routings {
            let filter = &self.filters[r.filter_idx];
            let in_spectra = &self.input_channels[r.in_ch].spectra_history;
            let scale = r.scale;
            let num_parts = p_tail.min(filter.tail_spectra.len());
            let target = if r.to_left {
                &mut self.accum_freq_l
            } else {
                &mut self.accum_freq_r
            };

            if (scale - 1.0).abs() < 0.001 {
                for p in 0..num_parts {
                    let x = &in_spectra[p];
                    let h = &filter.tail_spectra[p];
                    for k in 0..NUM_BINS {
                        target[k] += x[k] * h[k];
                    }
                }
            } else {
                for p in 0..num_parts {
                    let x = &in_spectra[p];
                    let h = &filter.tail_spectra[p];
                    for k in 0..NUM_BINS {
                        target[k] += (x[k] * h[k]) * scale;
                    }
                }
            }
        }

        // 3. Exactly TWO inverse FFTs: one for Left, one for Right
        let norm_scale = 1.0 / (fft_size as f32);

        self.c2r
            .process(&mut self.accum_freq_l, &mut self.ifft_scratch_time)
            .unwrap();
        for i in 0..b {
            self.tail_block_l[i] = self.ifft_scratch_time[b + i] * norm_scale;
        }

        self.c2r
            .process(&mut self.accum_freq_r, &mut self.ifft_scratch_time)
            .unwrap();
        for i in 0..b {
            self.tail_block_r[i] = self.ifft_scratch_time[b + i] * norm_scale;
        }
    }
}

/// Natural acoustic crossfeed and stereo width processor.
///
/// Implements frequency-dependent acoustic head shadowing with an interaural time delay (~260 µs)
/// and a gentle low-pass shelf (~700 Hz cutoff) along with Mid/Side stereo width adjustment.
#[derive(Clone)]
pub struct SpatialProcessor {
    delay_l: [f32; 64],
    delay_r: [f32; 64],
    delay_ptr: usize,
    delay_len: usize,
    lpf_l: f32,
    lpf_r: f32,
    lpf_alpha: f32,
}

impl SpatialProcessor {
    pub fn new(rate: u32) -> Self {
        let mut p = SpatialProcessor {
            delay_l: [0.0; 64],
            delay_r: [0.0; 64],
            delay_ptr: 0,
            delay_len: 13,
            lpf_l: 0.0,
            lpf_r: 0.0,
            lpf_alpha: 0.1,
        };
        p.reset(rate);
        p
    }

    pub fn reset(&mut self, rate: u32) {
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.lpf_l = 0.0;
        self.lpf_r = 0.0;
        let rate_f = rate.max(8000) as f32;
        // ~260 microseconds interaural head delay
        self.delay_len = ((0.00026 * rate_f).round() as usize).clamp(1, 63);
        // ~700 Hz low-pass cutoff for head shadow simulation
        let fc = 700.0f32;
        let dt = 1.0 / rate_f;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
        self.lpf_alpha = (dt / (rc + dt)).clamp(0.01, 0.99);
    }

    /// Process a stereo frame through crossfeed and stereo width.
    #[inline(always)]
    pub fn process_frame(&mut self, l: f32, r: f32, crossfeed: f32, width: f32) -> (f32, f32) {
        let mut out_l = l;
        let mut out_r = r;

        if crossfeed > 0.001 {
            let read_idx = (self.delay_ptr + 64 - self.delay_len) % 64;
            let del_l = self.delay_l[read_idx];
            let del_r = self.delay_r[read_idx];

            self.delay_l[self.delay_ptr] = l;
            self.delay_r[self.delay_ptr] = r;
            self.delay_ptr = (self.delay_ptr + 1) % 64;

            self.lpf_l += self.lpf_alpha * (del_l - self.lpf_l);
            self.lpf_r += self.lpf_alpha * (del_r - self.lpf_r);

            // Crossfeed level up to -4.5 dB
            let cross_gain = crossfeed * 0.45;
            let cross_l = l + cross_gain * self.lpf_r;
            let cross_r = r + cross_gain * self.lpf_l;
            let norm = 1.0 / (1.0 + cross_gain * 0.7);
            out_l = cross_l * norm;
            out_r = cross_r * norm;
        }

        if (width - 1.0).abs() > 0.001 {
            let mid = 0.5 * (out_l + out_r);
            let side = 0.5 * (out_l - out_r) * width;
            out_l = mid + side;
            out_r = mid - side;
        }

        (out_l, out_r)
    }
}

/// The 7 discrete surround speaker points (HeSuVi / 7.1 layout):
/// FL, FR, FC, SL, SR, BL, BR.
pub const SURROUND_POINTS: usize = 7;
pub const POINT_FL: usize = 0;
pub const POINT_FR: usize = 1;
pub const POINT_FC: usize = 2;
pub const POINT_SL: usize = 3;
pub const POINT_SR: usize = 4;
pub const POINT_BL: usize = 5;
pub const POINT_BR: usize = 6;

/// Shared parameters between the UI and the [`Convolver`] node running on the decode thread.
pub struct ConvolverParams {
    enabled: AtomicBool,
    /// Wet mix factor (0.0 to 1.0), stored as f32 bits.
    wet: AtomicU32,
    /// Output gain in dB (-12.0 to +12.0), stored as f32 bits.
    gain_db: AtomicU32,
    /// Operating mode for HeSuVi profiles (VirtualStereo or Surround7_1).
    mode: AtomicU8,
    /// Stereo width (0.0 = mono, 1.0 = normal, 2.0 = ultra-wide), stored as f32 bits.
    stereo_width: AtomicU32,
    /// Crossfeed intensity (0.0 = off, 1.0 = full), stored as f32 bits.
    crossfeed: AtomicU32,
    /// Volume adjustments in dB for the 7 surround speaker positions:
    /// [FL, FR, FC, SL, SR, BL, BR].
    channel_gains_db: [AtomicU32; 7],
    /// Currently loaded impulse response, if any.
    ir: RwLock<Option<Arc<WavIr>>>,
    /// Bumped by set_ir so the decode thread can cheaply detect changes
    /// without taking the lock on every buffer.
    ir_gen: AtomicU64,
}

impl ConvolverParams {
    pub fn new(
        enabled: bool,
        wet: f32,
        gain_db: f32,
        mode: ConvolverMode,
        stereo_width: f32,
        crossfeed: f32,
        channel_gains_db: Option<&[f32]>,
        ir: Option<WavIr>,
    ) -> ConvolverParams {
        let default_gains = [0.0f32; 7];
        let gains = channel_gains_db.unwrap_or(&default_gains);
        ConvolverParams {
            enabled: AtomicBool::new(enabled),
            wet: AtomicU32::new(wet.clamp(0.0, 1.0).to_bits()),
            gain_db: AtomicU32::new(gain_db.clamp(-12.0, 12.0).to_bits()),
            mode: AtomicU8::new(mode as u8),
            stereo_width: AtomicU32::new(stereo_width.clamp(0.0, 2.0).to_bits()),
            crossfeed: AtomicU32::new(crossfeed.clamp(0.0, 1.0).to_bits()),
            channel_gains_db: std::array::from_fn(|i| {
                let g = gains.get(i).copied().unwrap_or(0.0);
                AtomicU32::new(g.clamp(-24.0, 12.0).to_bits())
            }),
            ir: RwLock::new(ir.map(Arc::new)),
            ir_gen: AtomicU64::new(0),
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn wet(&self) -> f32 {
        f32::from_bits(self.wet.load(Ordering::Relaxed))
    }

    pub fn set_wet(&self, wet: f32) {
        self.wet.store(wet.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn gain_db(&self) -> f32 {
        f32::from_bits(self.gain_db.load(Ordering::Relaxed))
    }

    pub fn set_gain_db(&self, db: f32) {
        self.gain_db
            .store(db.clamp(-12.0, 12.0).to_bits(), Ordering::Relaxed);
    }

    pub fn gain_linear(&self) -> f32 {
        let db = self.gain_db();
        if db.abs() < 0.01 {
            1.0
        } else {
            10.0f32.powf(db / 20.0)
        }
    }

    pub fn mode(&self) -> ConvolverMode {
        ConvolverMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_mode(&self, mode: ConvolverMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    pub fn stereo_width(&self) -> f32 {
        f32::from_bits(self.stereo_width.load(Ordering::Relaxed))
    }

    pub fn set_stereo_width(&self, width: f32) {
        self.stereo_width
            .store(width.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn crossfeed(&self) -> f32 {
        f32::from_bits(self.crossfeed.load(Ordering::Relaxed))
    }

    pub fn set_crossfeed(&self, crossfeed: f32) {
        self.crossfeed
            .store(crossfeed.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn channel_gain_db(&self, ch: usize) -> f32 {
        if ch < SURROUND_POINTS {
            f32::from_bits(self.channel_gains_db[ch].load(Ordering::Relaxed))
        } else {
            0.0
        }
    }

    pub fn set_channel_gain_db(&self, ch: usize, db: f32) {
        if ch < SURROUND_POINTS {
            self.channel_gains_db[ch].store(db.clamp(-24.0, 12.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn channel_gain_linear(&self, ch: usize) -> f32 {
        let db = self.channel_gain_db(ch);
        if db.abs() < 0.01 {
            1.0
        } else {
            10.0f32.powf(db / 20.0)
        }
    }

    pub fn all_channel_gains_linear(&self) -> [f32; SURROUND_POINTS] {
        std::array::from_fn(|i| self.channel_gain_linear(i))
    }

    /// Read the current IR generation counter (cheap atomic load).
    pub fn ir_gen(&self) -> u64 {
        self.ir_gen.load(Ordering::Acquire)
    }

    /// Clone the current IR Arc. Only called when the generation changed.
    pub fn current_ir(&self) -> Option<Arc<WavIr>> {
        self.ir.read().ok()?.clone()
    }

    pub fn set_ir(&self, ir: Option<WavIr>) {
        if let Ok(mut lock) = self.ir.write() {
            *lock = ir.map(Arc::new);
        }
        self.ir_gen.fetch_add(1, Ordering::Release);
    }
}

/// The Convolver DSP node, implementing [`Node`] for inclusion in [`crate::chain::Chain`].
pub struct Convolver {
    params: Arc<ConvolverParams>,
    active_ir: Option<Arc<WavIr>>,
    rate: u32,
    engine: Option<PartitionedConvolver>,
    spatial: SpatialProcessor,
    seen_ir_gen: u64,
    seen_mode: ConvolverMode,
    r2c: Arc<dyn RealToComplex<f32>>,
    c2r: Arc<dyn ComplexToReal<f32>>,
}

impl Convolver {
    pub fn new(params: Arc<ConvolverParams>) -> Convolver {
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(FFT_LEN);
        let c2r = planner.plan_fft_inverse(FFT_LEN);
        Convolver {
            params,
            active_ir: None,
            rate: 0,
            engine: None,
            spatial: SpatialProcessor::new(48000),
            seen_ir_gen: u64::MAX,
            seen_mode: ConvolverMode::VirtualStereo,
            r2c,
            c2r,
        }
    }

    fn rebuild_filters(&mut self) {
        if self.rate == 0 {
            self.engine = None;
            return;
        }
        let Some(ir) = &self.active_ir else {
            self.engine = None;
            return;
        };
        // Resample IR to device rate if needed
        let ready_ir = if ir.sample_rate != self.rate {
            ir.resampled_to(self.rate)
        } else {
            (**ir).clone()
        };
        let mode = self.params.mode();
        self.seen_mode = mode;
        self.engine = Some(PartitionedConvolver::new(
            &ready_ir,
            mode,
            self.r2c.clone(),
            self.c2r.clone(),
        ));
    }
}

impl Node for Convolver {
    fn reset(&mut self, rate: u32) {
        self.rate = rate;
        self.spatial.reset(rate);
        self.active_ir = self.params.current_ir();
        self.seen_ir_gen = self.params.ir_gen();
        self.seen_mode = self.params.mode();
        self.rebuild_filters();
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.rate == 0 || !self.params.enabled() {
            if let Some(engine) = &mut self.engine {
                engine.clear();
            }
            return;
        }

        // Check if loaded IR or mode has changed
        let ir_gen_now = self.params.ir_gen();
        let mode = self.params.mode();
        if ir_gen_now != self.seen_ir_gen || mode != self.seen_mode {
            self.seen_ir_gen = ir_gen_now;
            self.seen_mode = mode;
            self.active_ir = self.params.current_ir();
            self.rebuild_filters();
        }

        let wet = self.params.wet();
        let gain = self.params.gain_linear();
        let width = self.params.stereo_width();
        let crossfeed = self.params.crossfeed();
        let has_ir = self.engine.is_some();
        let channel_gains = self.params.all_channel_gains_linear();

        for chunk in buf.chunks_exact_mut(2) {
            let in_l = chunk[0];
            let in_r = chunk[1];

            let (wet_l, wet_r) = match &mut self.engine {
                Some(conv) => conv.process_frame(in_l, in_r, &channel_gains),
                None => self.spatial.process_frame(in_l, in_r, crossfeed, width),
            };

            // Post-convolution stereo width if an IR was used
            let (final_wet_l, final_wet_r) = if has_ir && (width - 1.0).abs() > 0.001 {
                let mid = 0.5 * (wet_l + wet_r);
                let side = 0.5 * (wet_l - wet_r) * width;
                (mid + side, mid - side)
            } else {
                (wet_l, wet_r)
            };

            // Wet/dry mix and output gain
            let out_l = ((1.0 - wet) * in_l + wet * final_wet_l) * gain;
            let out_r = ((1.0 - wet) * in_r + wet * final_wet_r) * gain;

            chunk[0] = out_l;
            chunk[1] = out_r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Chain;

    const RATE: u32 = 48000;

    fn make_test_wav(channels: u16, rate: u32, samples_per_channel: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        // RIFF header
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder
        buf.extend_from_slice(b"WAVE");

        // fmt chunk (IEEE Float)
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&18u32.to_le_bytes()); // chunk size
        buf.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&rate.to_le_bytes());
        let byte_rate = rate * (channels as u32) * 4;
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        let block_align = channels * 4;
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&32u16.to_le_bytes()); // bits per sample
        buf.extend_from_slice(&0u16.to_le_bytes()); // cbSize

        // data chunk
        buf.extend_from_slice(b"data");
        let data_size = (samples_per_channel * block_align as usize) as u32;
        buf.extend_from_slice(&data_size.to_le_bytes());

        for frame in 0..samples_per_channel {
            for ch in 0..channels {
                // An impulse at frame 0, then silence
                let val: f32 = if frame == 0 {
                    1.0 / (ch as f32 + 1.0)
                } else {
                    0.0
                };
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }

        // Fill in total RIFF size
        let total_size = (buf.len() - 8) as u32;
        buf[4..8].copy_from_slice(&total_size.to_le_bytes());
        buf
    }

    #[test]
    fn parse_wav_14_channel_hesuvi() {
        let wav_bytes = make_test_wav(14, 48000, 64);
        let ir = parse_wav("test_hesuvi.wav", &wav_bytes).expect("parse failed");
        assert_eq!(ir.layout, IrLayout::Hesuvi14);
        assert_eq!(ir.channels.len(), 14);
        assert_eq!(ir.sample_rate, 48000);
        assert_eq!(ir.channels[0].len(), 64);
    }

    #[test]
    fn parse_wav_true_stereo_4ch() {
        let wav_bytes = make_test_wav(4, 44100, 32);
        let ir = parse_wav("true_stereo.wav", &wav_bytes).expect("parse failed");
        assert_eq!(ir.layout, IrLayout::TrueStereo4);
        assert_eq!(ir.channels.len(), 4);
        assert_eq!(ir.sample_rate, 44100);
    }

    #[test]
    fn disabled_convolver_is_bit_exact_passthrough() {
        let params = Arc::new(ConvolverParams::new(
            false,
            1.0,
            0.0,
            ConvolverMode::VirtualStereo,
            1.0,
            0.0,
            None,
            None,
        ));
        let mut chain = Chain::new();
        chain.push(Box::new(Convolver::new(params)));
        chain.reset(RATE);

        let original = vec![0.123f32, -0.456, 0.789, -0.987, 0.0, 1.0];
        let mut buf = original.clone();
        chain.process(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn impulse_response_convolution_yields_exact_coefficients() {
        let wav_bytes = make_test_wav(2, RATE, 16);
        let ir = parse_wav("stereo.wav", &wav_bytes).unwrap();
        let params = Arc::new(ConvolverParams::new(
            true,
            1.0,
            0.0,
            ConvolverMode::VirtualStereo,
            1.0,
            0.0,
            None,
            Some(ir.clone()),
        ));

        let mut node = Convolver::new(params);
        node.reset(RATE);

        // Input an impulse in L and R: [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, ...]
        let mut buf = vec![0.0f32; 32];
        buf[0] = 1.0;
        buf[1] = 1.0;
        node.process(&mut buf);

        // The first sample output should match the first coefficient of the IR
        assert!((buf[0] - ir.channels[0][0]).abs() < 1e-5);
        assert!((buf[1] - ir.channels[1][0]).abs() < 1e-5);
    }

    #[test]
    fn spatial_crossfeed_and_width_alters_soundstage() {
        let params = Arc::new(ConvolverParams::new(
            true,
            1.0,
            0.0,
            ConvolverMode::VirtualStereo,
            1.5, // 150% stereo width
            0.8, // 80% crossfeed
            None,
            None,
        ));
        let mut node = Convolver::new(params);
        node.reset(RATE);

        // Hard-panned left signal
        let mut buf = vec![0.0f32; 64];
        for i in (0..buf.len()).step_by(2) {
            buf[i] = 0.5;
            buf[i + 1] = 0.0;
        }
        node.process(&mut buf);

        // Crossfeed should have introduced signal into the right channel
        let right_energy: f32 = buf.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(right_energy > 0.01, "Crossfeed must leak signal to right ear");
    }

    #[test]
    fn hesuvi_surround_upmix_convolves_all_channels() {
        let wav_bytes = make_test_wav(14, RATE, 32);
        let ir = parse_wav("hesuvi.wav", &wav_bytes).unwrap();
        let params = Arc::new(ConvolverParams::new(
            true,
            1.0,
            0.0,
            ConvolverMode::Surround7_1,
            1.0,
            0.0,
            None,
            Some(ir),
        ));
        let mut node = Convolver::new(params);
        node.reset(RATE);

        let mut buf = vec![0.0f32; 64];
        buf[0] = 0.8;
        buf[1] = 0.6;
        node.process(&mut buf);

        assert!(buf[0].abs() > 0.001);
        assert!(buf[1].abs() > 0.001);
    }

    fn make_test_pcm_wav(channels: u16, rate: u32, bits: u16, samples: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"WAVE");

        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&rate.to_le_bytes());
        let bytes_per_sample = (bits / 8) as u32;
        let block_align = channels * (bits / 8);
        let byte_rate = rate * (channels as u32) * bytes_per_sample;
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits.to_le_bytes());

        buf.extend_from_slice(b"data");
        let data_size = (samples * block_align as usize) as u32;
        buf.extend_from_slice(&data_size.to_le_bytes());

        for frame in 0..samples {
            for ch in 0..channels {
                let sample_val = if frame == 0 && ch == 0 { 0.5f32 } else { 0.0f32 };
                match bits {
                    16 => {
                        let v = (sample_val * 32767.0) as i16;
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    24 => {
                        let v = (sample_val * 8388607.0) as i32;
                        buf.push((v & 0xFF) as u8);
                        buf.push(((v >> 8) & 0xFF) as u8);
                        buf.push(((v >> 16) & 0xFF) as u8);
                    }
                    32 => {
                        let v = (sample_val * 2147483647.0) as i32;
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    _ => panic!("unsupported bits"),
                }
            }
        }
        let total_size = (buf.len() - 8) as u32;
        buf[4..8].copy_from_slice(&total_size.to_le_bytes());
        buf
    }

    #[test]
    fn parse_wav_16bit_pcm() {
        let wav = make_test_pcm_wav(2, 44100, 16, 10);
        let ir = parse_wav("pcm16.wav", &wav).expect("16-bit PCM WAV failed to parse");
        assert_eq!(ir.layout, IrLayout::Stereo2);
        assert_eq!(ir.sample_rate, 44100);
        assert_eq!(ir.channels.len(), 2);
        assert!((ir.channels[0][0] - 0.5).abs() < 0.05);
    }

    #[test]
    fn parse_wav_24bit_pcm() {
        let wav = make_test_pcm_wav(2, 48000, 24, 10);
        let ir = parse_wav("pcm24.wav", &wav).expect("24-bit PCM WAV failed to parse");
        assert_eq!(ir.layout, IrLayout::Stereo2);
        assert_eq!(ir.sample_rate, 48000);
        assert!((ir.channels[0][0] - 0.5).abs() < 0.05);
    }

    #[test]
    fn ir_resampling_converts_rates() {
        let wav = make_test_pcm_wav(2, 44100, 16, 44);
        let ir = parse_wav("resample.wav", &wav).unwrap();
        let resampled = ir.resampled_to(48000);
        assert_eq!(resampled.sample_rate, 48000);
        assert_eq!(resampled.channels.len(), 2);
        let expected_len = (44 * 48000) / 44100;
        assert_eq!(resampled.channels[0].len(), expected_len);
    }

    #[test]
    fn wet_dry_mix_and_gain_scaling() {
        let params = Arc::new(ConvolverParams::new(
            true,
            0.5, // 50% wet
            6.0, // +6 dB gain (~2.0 linear)
            ConvolverMode::VirtualStereo,
            1.0,
            0.0,
            None,
            None,
        ));
        let mut node = Convolver::new(params);
        node.reset(RATE);

        let mut buf = vec![0.5f32; 8];
        node.process(&mut buf);
        // Gain ~ 1.995 * 0.5 ~ 1.0
        assert!((buf[0] - 1.0).abs() < 0.05);
    }

    #[test]
    fn test_all_builtin_profiles_load_and_convolve() {
        for &profile in BuiltinHesuviProfile::all() {
            if profile == BuiltinHesuviProfile::None {
                assert!(profile.load_ir().is_none());
                continue;
            }
            let ir = profile
                .load_ir()
                .unwrap_or_else(|| panic!("Failed to load profile {:?}", profile));
            assert_eq!(ir.layout, IrLayout::Hesuvi14);
            assert_eq!(ir.channels.len(), 14);
            assert_eq!(ir.sample_rate, 48000);

            let params = Arc::new(ConvolverParams::new(
                true,
                1.0,
                0.0,
                ConvolverMode::VirtualStereo,
                1.0,
                0.0,
                None,
                Some(ir),
            ));
            let mut node = Convolver::new(params);
            node.reset(48000);
            let mut buf = vec![0.5f32; 32];
            node.process(&mut buf);
            assert!(buf[0].is_finite());
        }
    }

    #[test]
    fn test_surround_individual_channel_volume_scaling() {
        let wav_bytes = make_test_wav(14, RATE, 64);
        let ir = parse_wav("hesuvi.wav", &wav_bytes).unwrap();
        // Center channel boosted +6 dB, side channels muted (-24 dB)
        let mut gains = [0.0f32; 7];
        gains[POINT_FC] = 6.0;
        gains[POINT_SL] = -24.0;
        gains[POINT_SR] = -24.0;

        let params = Arc::new(ConvolverParams::new(
            true,
            1.0,
            0.0,
            ConvolverMode::Surround7_1,
            1.0,
            0.0,
            Some(&gains),
            Some(ir),
        ));

        assert_eq!(params.channel_gain_db(POINT_FC), 6.0);
        assert_eq!(params.channel_gain_db(POINT_SL), -24.0);
        assert!((params.channel_gain_linear(POINT_FC) - 1.995).abs() < 0.05);

        let mut node = Convolver::new(params.clone());
        node.reset(RATE);

        let mut buf = vec![0.5f32; 64];
        node.process(&mut buf);
        assert!(buf[0].is_finite());

        // Changing live gain is atomic
        params.set_channel_gain_db(POINT_FC, -12.0);
        assert_eq!(params.channel_gain_db(POINT_FC), -12.0);
    }
}
