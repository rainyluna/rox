//! Opus decode, the half Symphonia doesn't ship. `symphonia-format-ogg` reads
//! an Ogg Opus stream all the way to a track: 48 kHz, channels off the mapping
//! family, the whole `OpusHead` handed over as extra data. Then the open fails,
//! because no decoder in symphonia 0.6 claims `CODEC_ID_OPUS`. rox's own
//! converter writes `.opus` files, so that gap meant the app produced files it
//! then refused to index or play. This module closes it with `opus-pure`,
//! which has no dependencies at all and no build script, so ADR 2's "no C
//! dependency in the decode path" still holds. It isn't `unsafe`-free: about
//! 130 blocks are AVX2 and NEON intrinsics, each behind a `#[target_feature]`
//! function that only runs after the matching runtime detection. That's the
//! ordinary shape of SIMD in Rust and it isn't C, but it's worth naming rather
//! than claiming a clean bill.
//!
//! The crate was picked by measurement, not reputation. Decoding fourteen real
//! 192 kbit/s tracks packet by packet and diffing against libopus through
//! ffmpeg, it lands between -89 and -126 dB of error, better on every single
//! file than any other pure-Rust decoder on crates.io, most of which are wrong
//! by 10 to 20 dB or produce silence outright. `tests/fixtures/README.md`
//! records the comparison and the decoder this one replaced.
//!
//! What it deliberately doesn't do: multistream Opus, meaning channel mapping
//! family 1 with more than two channels, so a 5.1 file refuses to open and
//! says why in the log. Refusing is the honest answer; downmixing a layout we
//! can't decode would be worse. The crate does ship an `OpusMSDecoder`, so
//! this is a wiring job somebody can pick up, not a wall.
//!
//! Gapless (ADR 3) is the decoder's job, and for Opus the two trims arrive by
//! different routes. The end padding comes in on the last packet's `trim_end`,
//! which the Ogg reader works out from the granule position. The pre-skip does
//! not: the Ogg mapping's packet parser returns a zero discard on every path,
//! so `trim_start` is always zero for Opus, unlike Vorbis. So this module reads
//! the pre-skip out of the `OpusHead` and drops those frames itself, keyed off
//! `packet.pts` so a seek into the middle of a file can't re-trigger it. Skip
//! either half and every track boundary picks up a click.
//!
//! One known rough edge. RFC 7845 asks for 80 ms of pre-roll after a seek
//! before the output is exact, and the Ogg reader seeks to a page boundary and
//! hands over whatever it lands on. The first frames after a seek can be
//! slightly degraded because of it. Left as is until somebody hears it.

use symphonia::core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia::core::codecs::CodecInfo;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::errors::{Result, decode_error, unsupported_error};
use symphonia::core::packet::PacketRef;
use symphonia::core::support_audio_codec;

/// Frames per channel in the longest packet Opus allows: 120 ms at 48 kHz.
/// Both the decode buffer and the interleaved scratch are sized off this once
/// at open, so nothing on the decode path ever grows a Vec.
const MAX_FRAMES_PER_PACKET: usize = 5760;

/// The playback rate for every Opus stream. The rate in the `OpusHead` is what
/// the encoder was fed, not what comes out; Opus always decodes at 48 kHz.
const OPUS_RATE: u32 = 48_000;

/// A single-stream Opus decoder wired into Symphonia's decoder trait.
pub struct OpusDecoder {
    params: AudioCodecParameters,
    inner: opus_pure::OpusDecoder,
    /// The `OpusHead` output gain as a linear factor, `None` when the header
    /// asks for 0 dB. `None` rather than `Some(1.0)` so the common case is an
    /// exact passthrough with no multiply at all, the way `eq.rs` keeps a flat
    /// band exact.
    gain: Option<f32>,
    /// Frames of encoder delay to drop off the front of the stream.
    pre_skip: u64,
    /// Whether to apply the trims. Off only if a caller explicitly disables
    /// gapless; rox never does, but the option is part of the trait's contract.
    gapless: bool,
    channels: usize,
    buf: AudioBuffer<f32>,
    scratch: Vec<f32>,
}

impl OpusDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let Some(channels) = params.channels.clone() else {
            return unsupported_error("opus: no channel layout");
        };
        let count = channels.count();
        if count == 0 || count > 2 {
            return unsupported_error("opus: multistream channel layouts");
        }

        let inner = match opus_pure::OpusDecoder::new(OPUS_RATE as i32, count) {
            Ok(inner) => inner,
            Err(_) => return decode_error("opus: decoder rejected the stream parameters"),
        };

        let (pre_skip, gain) = parse_head(params.extra_data.as_deref().unwrap_or(&[]));

        Ok(OpusDecoder {
            params: params.clone(),
            inner,
            gain,
            pre_skip,
            gapless: opts.gapless,
            channels: count,
            buf: AudioBuffer::new(AudioSpec::new(OPUS_RATE, channels), MAX_FRAMES_PER_PACKET),
            scratch: vec![0.0; MAX_FRAMES_PER_PACKET * count],
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // `frame_size` is the capacity the decoder may write, in frames per
        // channel, and the scratch is sized to exactly that at open, so this
        // never has to grow.
        let frames = match self
            .inner
            .decode(packet.data, MAX_FRAMES_PER_PACKET, &mut self.scratch)
        {
            Ok(frames) => frames.min(MAX_FRAMES_PER_PACKET),
            Err(e) => {
                log::debug!("opus: packet decode failed ({e})");
                return decode_error("opus: packet decode failed");
            }
        };

        // Interleaved out of the decoder, planar into the buffer, gain folded
        // into the same pass when the header asked for one.
        let Self {
            buf,
            scratch,
            gain,
            channels,
            ..
        } = self;
        buf.clear();
        buf.render_uninit(Some(frames));
        for ch in 0..*channels {
            let Some(plane) = buf.plane_mut(ch) else {
                continue;
            };
            let src = scratch[ch..].iter().step_by(*channels);
            match gain {
                Some(g) => {
                    for (dst, s) in plane.iter_mut().zip(src) {
                        *dst = *s * *g;
                    }
                }
                None => {
                    for (dst, s) in plane.iter_mut().zip(src) {
                        *dst = *s;
                    }
                }
            }
        }

        if self.gapless {
            // The pre-skip is measured from the start of the stream, and the
            // reader counts pts from zero including it, so what's left to drop
            // is whatever the packet's own timestamp hasn't already covered.
            // After a seek the pts is past the pre-skip and this is zero.
            let pts = packet.pts.get().max(0) as u64;
            let skip = self.pre_skip.saturating_sub(pts).min(frames as u64) as usize;
            self.buf.trim(skip, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

/// The pre-skip and output gain out of an `OpusHead` (RFC 7845 section 5.1):
/// a little-endian u16 of 48 kHz frames at byte 10, and a little-endian i16 of
/// Q7.8 dB at byte 16. A header too short to hold either reads as zero, which
/// plays the same as a file asking for neither.
fn parse_head(head: &[u8]) -> (u64, Option<f32>) {
    let pre_skip = head
        .get(10..12)
        .map(|b| u64::from(u16::from_le_bytes([b[0], b[1]])))
        .unwrap_or(0);
    let q78 = head
        .get(16..18)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    let gain = (q78 != 0).then(|| 10f32.powf(f32::from(q78) / 256.0 / 20.0));
    (pre_skip, gain)
}

impl AudioDecoder for OpusDecoder {
    /// Opus packets decode independently, so unlike Vorbis there's no overlap
    /// with a neighbour to reconstruct and no reason to silence the first
    /// packet after a reset.
    fn reset(&mut self) {
        // `reset_state` rebuilds the decoder from the rate and channel count it
        // already holds, so the only error it can return is one those two would
        // have failed construction with. It can't happen from here, but the
        // trait gives us nowhere to report it, so say so in the log and carry
        // the old state rather than swallowing it silently.
        if let Err(e) = self.inner.reset_state() {
            log::warn!("opus: decoder reset failed ({e})");
        }
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().unwrap().info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        match self.decode_inner(packet) {
            Ok(()) => Ok(self.buf.as_generic_audio_buffer_ref()),
            Err(e) => {
                self.buf.clear();
                Err(e)
            }
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for OpusDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(OpusDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        // The macro spells the crate out as `symphonia_core`, and rox depends
        // on the `symphonia` facade rather than the core crate directly, so
        // the name has to exist locally for the expansion to resolve.
        use symphonia::core as symphonia_core;
        &[support_audio_codec!(CODEC_ID_OPUS, "opus", "Opus")]
    }
}

#[cfg(test)]
mod opus_tests {
    use super::*;
    use symphonia::core::audio::{Channels, Position};

    /// A 19-byte `OpusHead`: magic, version 1, `channels`, `pre_skip`, input
    /// rate 48000, `gain` in Q7.8 dB, and the mapping family.
    fn head(channels: u8, pre_skip: u16, gain: i16, family: u8) -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.push(1);
        h.push(channels);
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&gain.to_le_bytes());
        h.push(family);
        h
    }

    fn params(channels: Channels, head: &[u8]) -> AudioCodecParameters {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_OPUS)
            .with_sample_rate(OPUS_RATE)
            .with_channels(channels)
            .with_extra_data(Box::from(head));
        p
    }

    #[test]
    fn the_header_gain_reads_as_a_linear_factor() {
        let (pre_skip, gain) = parse_head(&head(2, 312, 256, 0));
        assert_eq!(pre_skip, 312, "pre-skip off bytes 10..12");
        let gain = gain.expect("+1 dB is a real gain");
        assert!(
            (gain - 1.122_018_5).abs() < 1e-5,
            "+1 dB in Q7.8 is a factor of about 1.122, got {gain}"
        );
    }

    /// The overwhelmingly common case, and the one that has to stay exact:
    /// a zero header gain must not become a multiply by a rounded 1.0.
    #[test]
    fn a_zero_header_gain_is_no_gain_at_all() {
        let (_, gain) = parse_head(&head(2, 312, 0, 0));
        assert_eq!(gain, None, "0 dB is a passthrough, not a factor");
    }

    /// A header shorter than the fields is a broken file, not a crash.
    #[test]
    fn a_truncated_header_reads_as_no_skip_and_no_gain() {
        assert_eq!(parse_head(b"OpusHead"), (0, None));
        assert_eq!(parse_head(&[]), (0, None));
    }

    /// Mapping family 1 with six channels is multistream, which this decoder
    /// can't do. It has to refuse at open rather than play something wrong.
    #[test]
    fn a_multistream_layout_refuses_to_open() {
        let six = Channels::Positioned(
            Position::FRONT_LEFT
                | Position::FRONT_CENTER
                | Position::FRONT_RIGHT
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
                | Position::LFE1,
        );
        let params = params(six, &head(6, 312, 0, 1));
        let err = OpusDecoder::try_new(&params, &AudioDecoderOptions::default())
            .err()
            .expect("six channels is multistream");
        assert!(
            err.to_string().contains("multistream"),
            "the refusal names the reason, got {err}"
        );
    }

    #[test]
    fn a_stereo_stream_opens() {
        let stereo = Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT);
        let params = params(stereo, &head(2, 312, 0, 0));
        let dec = OpusDecoder::try_new(&params, &AudioDecoderOptions::default())
            .expect("stereo family 0 is the ordinary case");
        assert_eq!(dec.pre_skip, 312);
        assert_eq!(dec.channels, 2);
    }

    /// The fixture: one second of a 440 Hz sine, checked in beside this crate's
    /// tests with the ffmpeg command in the README there.
    /// The tone fixture as the decode window wants it: a local locator, since
    /// the engine asks where a track's bytes come from now.
    fn fixture() -> rox_library::locator::Locator {
        named("tone-440.opus")
    }

    fn named(name: &str) -> rox_library::locator::Locator {
        rox_library::locator::Locator::Local(fixtures().join(name))
    }

    fn fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// The whole gapless story in one number. The file holds 48960 decoded
    /// frames: 312 of pre-skip, 48000 of tone, 648 of end padding. Getting
    /// exactly a second back means both trims landed, and the pre-skip one is
    /// the interesting half because the Ogg reader never signals it.
    #[test]
    fn the_fixture_decodes_to_exactly_one_second() {
        let audio = crate::engine::decode_window(&fixture(), 0.0, OPUS_RATE, 1_000_000)
            .expect("the fixture decodes");
        let frames = audio.len() / 2;
        assert!(
            frames.abs_diff(48_000) <= 1,
            "pre-skip and end padding both trimmed leaves one second, got {frames} frames"
        );
    }

    /// The transient fixture, and the reason it's checked in. A 440 Hz sine
    /// never makes CELT split a frame into short blocks, so the tone test
    /// above cannot reach the anti-collapse path at all. This file does: it
    /// holds a band at sixteen blocks, which is the case that overflowed the
    /// previous decoder's `u8` collapse mask and panicked on all fourteen
    /// tracks of a real-music corpus. Verified against `opus-decoder` 0.1.1,
    /// which panics on this file at packet 75 of 101.
    ///
    /// Decoding to the right length is the assertion, because the failure it
    /// guards against is a panic partway through, which never reaches a count.
    #[test]
    fn the_transient_fixture_decodes_without_panicking() {
        let audio = crate::engine::decode_window(
            &named("dense-transients.opus"),
            0.0,
            OPUS_RATE,
            1_000_000,
        )
        .expect("the transient fixture decodes");
        let frames = audio.len() / 2;
        assert!(
            frames.abs_diff(96_000) <= 1,
            "two seconds at 48 kHz once both trims land, got {frames} frames"
        );
    }

    /// The frame count alone would pass on a decode that ran to the end and
    /// produced garbage, and a wrong anti-collapse mask is exactly that kind
    /// of failure: right length, wrong samples. ffmpeg reads this file at an
    /// RMS of 0.125 left and 0.126 right with a peak of 0.774, so bracketing
    /// the level checks against libopus rather than against a floor we picked.
    #[test]
    fn the_transient_fixture_decodes_to_the_level_libopus_reads() {
        let audio = crate::engine::decode_window(
            &named("dense-transients.opus"),
            0.0,
            OPUS_RATE,
            1_000_000,
        )
        .expect("the transient fixture decodes");
        let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
        assert!(
            (0.115..0.136).contains(&rms),
            "the level matches what ffmpeg reads out of the same file, RMS {rms}"
        );
        let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (0.70..0.85).contains(&peak),
            "the transients survive the decode, peak {peak}"
        );
    }

    /// A frame count alone would pass on a decode that "succeeded" into
    /// silence or noise, so this checks the audio is the tone it should be.
    /// The level is the sharper half: ffmpeg's own volumedetect reads this
    /// file at -24.1 dB mean, an RMS of 0.0624, and this decode lands on
    /// 0.0626. Bracketing that is a cross-check against a second decoder, not
    /// an arbitrary floor. (lavfi's sine generator runs near -18 dBFS, which
    /// is why the number is nowhere near a full-scale 0.707.)
    #[test]
    fn the_fixture_decodes_to_a_440_hz_tone() {
        let audio = crate::engine::decode_window(&fixture(), 0.0, OPUS_RATE, 1_000_000)
            .expect("the fixture decodes");
        let left: Vec<f32> = audio.iter().step_by(2).copied().collect();
        let rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
        assert!(
            (0.055..0.070).contains(&rms),
            "the level matches what ffmpeg reads out of the same file, RMS {rms}"
        );

        // Two crossings per cycle, over a second of audio, so crossings / 2 is
        // the frequency in Hz. Skip the first and last 10 ms so a codec's
        // attack and release can't skew the count.
        let skip = 480;
        let body = &left[skip..left.len() - skip];
        let crossings = body
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let hz = crossings as f32 / 2.0 / (body.len() as f32 / OPUS_RATE as f32);
        assert!(
            (hz - 440.0).abs() / 440.0 < 0.01,
            "the tone is 440 Hz within 1%, measured {hz}"
        );
    }
}
