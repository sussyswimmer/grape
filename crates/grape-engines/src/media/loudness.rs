//! Normalize Volume's loudness plan (D26), pure: the filters of its passes,
//! what an `ebur128` measurement printed, and what to do about it.
//!
//! The gain is constant, so nothing rides: a measuring pass (`ebur128`
//! with true peak, mono measured as dual mono) gives the integrated
//! loudness `I` and true peak `TP`, and with `G` = target − `I`:
//!
//! - no summary, or `I` ≤ −70 LUFS: too short or too quiet to measure;
//! - |`G`| ≤ 0.5 LU with `TP` under the ceiling: already there;
//! - `TP + G` under the ceiling: a linear `volume` gain ([`linear`]);
//! - otherwise the gain and an oversampled `alimiter` at the ceiling −0.5 dB
//!   ([`limited`]), measured again and corrected at most twice
//!   ([`correct`]), since limiting takes some of the loudness back.
//!
//! The ceiling is −1 dBTP for lossless outputs and −2 dBTP for MP3 and AAC,
//! which overshoot by about half a decibel once encoded. Not `loudnorm`:
//! its dynamic mode rides the gain toward the running loudness, resamples
//! to 192 kHz, and would bring libebur128's MIT notice into the build.

/// What an `ebur128` summary says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Measured {
    /// The integrated loudness, in LUFS.
    pub(crate) lufs: f64,
    /// The true peak, in dBTP; `-inf` for silence.
    pub(crate) peak: f64,
}

/// How quiet a measurement can be and still count: `ebur128` prints
/// −70.0 LUFS (its absolute gate) when nothing passed it.
const SILENT: f64 = -70.0;

/// Within this of the target, with the peaks under the ceiling, a file is
/// left alone: a lossy re-encode for less would cost more than it gives.
const NEAR: f64 = 0.5;

/// Within this of the target after a check pass, the gain is kept.
const CLOSE: f64 = 0.3;

/// What [`NEAR`] and [`CLOSE`] allow on top: `ebur128` prints a tenth of
/// a unit, so −16.3 is 0.3 from −16 although the subtraction says a hair
/// more.
const SLACK: f64 = 1e-9;

/// How many check passes the limited path may run.
pub(crate) const MAX_CHECKS: usize = 2;

/// Reads the last `ebur128` summary in `stderr` (the end of what FFmpeg
/// wrote at `-v info`, which also holds the input's tags): its `I:` line
/// under "Integrated loudness" and its `Peak:` line under "True peak".
///
/// A summary starts on a line of its own that begins `[Parsed_ebur128`;
/// a tag can't print one, since FFmpeg prints each line of a tag's value
/// after an indent and a `: `. `None` when there's no summary, or it's
/// cut short. The format is `%5.1f` in FFmpeg 6.1 and 9.0.
pub(crate) fn parse(stderr: &str) -> Option<Measured> {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| l.starts_with("[Parsed_ebur128") && l.trim_end().ends_with("Summary:"))?;
    let (mut lufs, mut peak) = (None, None);
    let mut section = "";
    for line in &lines[start + 1..] {
        // The next log line ends the block.
        if line.starts_with('[') {
            break;
        }
        let line = line.trim();
        if line.ends_with(':') {
            section = line;
            continue;
        }
        let value = || line.split_whitespace().nth(1)?.parse::<f64>().ok();
        match section {
            "Integrated loudness:" if line.starts_with("I:") => lufs = value(),
            "True peak:" if line.starts_with("Peak:") => peak = value(),
            _ => {}
        }
    }
    Some(Measured {
        lufs: lufs.filter(|l| !l.is_nan())?,
        peak: peak.filter(|p| !p.is_nan())?,
    })
}

/// What to do with a file after its measuring pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Plan {
    /// Silent, or too short to measure.
    TooQuiet,
    /// Within [`NEAR`] of the target, with the peaks under the ceiling.
    AtTarget,
    /// The gain in dB, with no limiter.
    Linear(f64),
    /// The gain in dB, with the limiter, to be checked.
    Limited(f64),
}

/// [`Plan`] for a file that measured `measured` (`None`: no summary),
/// for a `target` in LUFS and a true-peak `ceiling` in dBTP.
pub(crate) fn plan(measured: Option<Measured>, target: f64, ceiling: f64) -> Plan {
    let Some(Measured { lufs, peak }) = measured.filter(|m| m.lufs > SILENT) else {
        return Plan::TooQuiet;
    };
    let gain = target - lufs;
    if gain.abs() <= NEAR + SLACK && peak <= ceiling {
        Plan::AtTarget
    } else if peak + gain <= ceiling {
        Plan::Linear(gain)
    } else {
        Plan::Limited(gain)
    }
}

/// What a check pass of the limited path says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Correction {
    /// Write with this gain.
    Write(f64),
    /// Check this gain again.
    Check(f64),
}

/// After check pass number `checks` (1 or 2) with `gain` measured
/// `lufs`: keep the gain within [`CLOSE`] of the target, else add what it
/// fell short by, and check that unless [`MAX_CHECKS`] were run.
pub(crate) fn correct(target: f64, gain: f64, lufs: f64, checks: usize) -> Correction {
    let short = target - lufs;
    if short.abs() <= CLOSE + SLACK {
        Correction::Write(gain)
    } else if checks >= MAX_CHECKS {
        Correction::Write(gain + short)
    } else {
        Correction::Check(gain + short)
    }
}

/// The true-peak ceiling of an output: −1 dBTP for a lossless one, −2 for
/// MP3 and AAC.
pub(crate) fn ceiling(lossless: bool) -> f64 {
    if lossless {
        -1.0
    } else {
        -2.0
    }
}

/// `LINEAR(G)`: the gain in double precision.
pub(crate) fn linear(gain: f64) -> String {
    format!("aformat=sample_fmts=dbl,volume={gain:.2}dB:precision=double")
}

/// `LIMITED(G)`: [`linear`], then `alimiter` at the ceiling −0.5 dB at an
/// oversampled rate ([`oversampled`]), so it catches the peaks between
/// samples too, and back to `rate`. `level=0` keeps it from raising the
/// output to the limit; `latency=1` keeps the length exact.
pub(crate) fn limited(gain: f64, ceiling: f64, rate: u32) -> String {
    let limit = 10f64.powf((ceiling - 0.5) / 20.0);
    let limiter = format!("alimiter=limit={limit:.5}:attack=5:release=50:level=0:latency=1");
    match oversampled(rate) {
        Some(fast) => format!(
            "{},aresample={fast},{limiter},aresample={rate}",
            linear(gain)
        ),
        None => format!("{},{limiter}", linear(gain)),
    }
}

/// The rate the limiter runs at for audio at `rate`: 4× up to 48 kHz, 2×
/// up to 96 kHz. `None` above (no oversampling) or for an unknown rate.
fn oversampled(rate: u32) -> Option<u32> {
    match rate {
        1..=48_000 => Some(rate * 4),
        48_001..=96_000 => Some(rate * 2),
        _ => None,
    }
}

/// The measuring filter after `chain` (none for the first pass): true
/// peak, no per-frame log, and mono measured as dual mono (a mono file
/// at −16 then plays as loud as stereo at −16).
pub(crate) fn measure(chain: Option<&str>, mono: bool) -> String {
    let dual = if mono { ":dualmono=true" } else { "" };
    let ebur128 = format!("ebur128=peak=true:framelog=quiet{dual}");
    match chain {
        Some(chain) => format!("{chain},{ebur128}"),
        None => ebur128,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The end of FFmpeg 9.0.2's stderr for a measuring pass over a FLAC
    /// whose title is made to look like a summary.
    const SUMMARY_902: &str = "\
Input #0, flac, from 'file:/tmp/tone.flac':
  Metadata:
    title           : Summary: I: 5.0 LUFS Peak: 9.0 dBFS
    encoder         : Lavf63.1.102
  Duration: 00:00:03.00, start: 0.000000, bitrate: 170 kb/s
  Stream #0:0: Audio: flac, 48000 Hz, mono, s16
Stream mapping:
  Stream #0:0 -> #0:0 (flac (native) -> pcm_s16le (native))
Output #0, null, to 'pipe:':
  Metadata:
    title           : Summary: I: 5.0 LUFS Peak: 9.0 dBFS
    encoder         : Lavf63.1.102
  Stream #0:0: Audio: pcm_s16le, 48000 Hz, mono, s16, 768 kb/s
    Metadata:
      encoder         : Lavc63.1.102 pcm_s16le
[Parsed_ebur128_0 @ 0x7f0118001a80] Summary:

  Integrated loudness:
    I:         -21.1 LUFS
    Threshold: -31.1 LUFS

  Loudness range:
    LRA:        20.0 LU
    Threshold: -41.1 LUFS
    LRA low:   -41.1 LUFS
    LRA high:  -21.1 LUFS

  True peak:
    Peak:      -18.1 dBFS
[out#0/null @ 0x55df6d14f2c0] video:0KiB audio:281KiB subtitle:0KiB other streams:0KiB global headers:0KiB muxing overhead: unknown
size=N/A time=00:00:03.00 bitrate=N/A speed= 134x elapsed=0:00:00.02
";

    /// FFmpeg 6.1.1's for a silent WAV: its final stats come first.
    const SILENT_611: &str = "\
[aist#0:0/pcm_s16le @ 0x564b0a8d1900] Guessed Channel Layout: mono
Input #0, wav, from 'file:/tmp/silent.wav':
  Duration: 00:00:03.00, bitrate: 768 kb/s
  Stream #0:0: Audio: pcm_s16le ([1][0][0][0] / 0x0001), 48000 Hz, 1 channels, s16, 768 kb/s
[out#0/null @ 0x564b0a8aacc0] video:0kB audio:281kB subtitle:0kB other streams:0kB global headers:0kB muxing overhead: unknown
size=N/A time=00:00:02.90 bitrate=N/A speed= 126x
[Parsed_ebur128_0 @ 0x564b0a8db5c0] Summary:

  Integrated loudness:
    I:         -70.0 LUFS
    Threshold:   0.0 LUFS

  Loudness range:
    LRA:         0.0 LU
    Threshold:   0.0 LUFS
    LRA low:     0.0 LUFS
    LRA high:    0.0 LUFS

  True peak:
    Peak:       -inf dBFS
";

    #[test]
    fn the_last_summary_is_read_and_tags_cant_fake_one() {
        assert_eq!(
            parse(SUMMARY_902),
            Some(Measured {
                lufs: -21.1,
                peak: -18.1
            })
        );
        let silent = parse(SILENT_611).unwrap();
        assert_eq!(silent.lufs, -70.0);
        assert_eq!(silent.peak, f64::NEG_INFINITY);
        // A tag whose value has a line break prints the next line after an
        // indent and ": ", never at the start of a line.
        let forged = SUMMARY_902.replace(
            "    encoder         : Lavf63.1.102\n  Duration",
            "    comment         : x\n                    : [Parsed_ebur128_0 @ 0x1] Summary:\n\
             \x20                   :   Integrated loudness:\n\
             \x20                   :     I:  5.0 LUFS\n  Duration",
        );
        assert_eq!(parse(&forged), parse(SUMMARY_902));
        // Two summaries (a check pass's log after another): the last wins.
        let two = format!("{SILENT_611}{SUMMARY_902}");
        assert_eq!(parse(&two).unwrap().lufs, -21.1);
        // No summary, or one cut short, is none.
        assert_eq!(parse(""), None);
        let cut = &SUMMARY_902[..SUMMARY_902.find("  True peak:").unwrap()];
        assert_eq!(parse(cut), None);
        let no_i = SUMMARY_902.replace("    I:         -21.1 LUFS\n", "");
        assert_eq!(parse(&no_i), None);
        // CRLF line ends (Windows) read the same.
        assert_eq!(
            parse(&SUMMARY_902.replace('\n', "\r\n")),
            parse(SUMMARY_902)
        );
    }

    /// The gain of a linear or limited plan, to 0.01 dB, and whether it
    /// limits.
    fn gain(plan: Plan) -> Option<(f64, bool)> {
        let round = |g: f64| (g * 100.0).round() / 100.0;
        match plan {
            Plan::Linear(g) => Some((round(g), false)),
            Plan::Limited(g) => Some((round(g), true)),
            Plan::TooQuiet | Plan::AtTarget => None,
        }
    }

    #[test]
    fn the_plan_follows_the_table() {
        let m = |lufs, peak| Some(Measured { lufs, peak });
        // Silent, too short, or no summary.
        assert_eq!(plan(None, -16.0, -1.0), Plan::TooQuiet);
        assert_eq!(
            plan(m(-70.0, f64::NEG_INFINITY), -16.0, -1.0),
            Plan::TooQuiet
        );
        // Already there: within 0.5 LU with the peaks under the ceiling.
        assert_eq!(plan(m(-16.4, -3.0), -16.0, -1.0), Plan::AtTarget);
        assert_eq!(plan(m(-15.5, -1.0), -16.0, -1.0), Plan::AtTarget);
        // Turning it down brings the peaks under the ceiling.
        assert_eq!(gain(plan(m(-15.5, -0.9), -16.0, -1.0)), Some((-0.5, false)));
        assert_eq!(gain(plan(m(-16.4, -0.7), -16.0, -1.0)), Some((0.4, true)));
        assert_eq!(plan(m(-16.5, -3.0), -16.0, -1.0), Plan::AtTarget);
        assert_eq!(gain(plan(m(-16.6, -3.0), -16.0, -1.0)), Some((0.6, false)));
        // The design's measured sources (§5.4).
        assert_eq!(
            gain(plan(m(-34.1, -23.7), -16.0, -1.0)),
            Some((18.1, false))
        );
        assert_eq!(gain(plan(m(-21.8, -18.1), -16.0, -1.0)), Some((5.8, false)));
        assert_eq!(gain(plan(m(-13.5, 1.6), -16.0, -1.0)), Some((-2.5, true)));
        assert_eq!(gain(plan(m(-27.0, -2.9), -16.0, -1.0)), Some((11.0, true)));
        // Exactly at the ceiling is still linear; the lossy ceiling is
        // lower.
        assert_eq!(gain(plan(m(-18.0, -3.0), -16.0, -1.0)), Some((2.0, false)));
        assert_eq!(gain(plan(m(-18.0, -3.5), -16.0, -2.0)), Some((2.0, true)));
        // Another target.
        assert_eq!(plan(m(-23.2, -10.0), -23.0, -2.0), Plan::AtTarget);
    }

    #[test]
    fn checks_correct_by_what_the_limiter_took() {
        // peaky.wav (§5.4): G 11.0 measured −21.5, then 16.5 measured −17.0,
        // written at 17.5.
        assert_eq!(correct(-16.0, 11.0, -21.5, 1), Correction::Check(16.5));
        assert_eq!(correct(-16.0, 16.5, -17.0, 2), Correction::Write(17.5));
        // Within 0.3 LU the gain stays, at either check.
        assert_eq!(correct(-16.0, -2.5, -16.0, 1), Correction::Write(-2.5));
        assert_eq!(correct(-16.0, 4.0, -16.3, 1), Correction::Write(4.0));
        assert_eq!(correct(-16.0, 4.0, -15.7, 2), Correction::Write(4.0));
        assert!(
            matches!(correct(-16.0, 4.0, -16.31, 1), Correction::Check(g) if (g - 4.31).abs() < 1e-9)
        );
        assert_eq!(MAX_CHECKS, 2);
    }

    #[test]
    fn the_filters_are_exact() {
        assert_eq!(
            linear(5.8),
            "aformat=sample_fmts=dbl,volume=5.80dB:precision=double"
        );
        assert_eq!(
            linear(-2.456),
            "aformat=sample_fmts=dbl,volume=-2.46dB:precision=double"
        );
        let limiter = "alimiter=limit=0.84140:attack=5:release=50:level=0:latency=1";
        assert_eq!(
            limited(11.0, -1.0, 44_100),
            format!(
                "{},aresample=176400,{limiter},aresample=44100",
                linear(11.0)
            )
        );
        assert_eq!(
            limited(11.0, -1.0, 48_000),
            format!(
                "{},aresample=192000,{limiter},aresample=48000",
                linear(11.0)
            )
        );
        assert_eq!(
            limited(11.0, -1.0, 96_000),
            format!(
                "{},aresample=192000,{limiter},aresample=96000",
                linear(11.0)
            )
        );
        assert_eq!(
            limited(11.0, -1.0, 192_000),
            format!("{},{limiter}", linear(11.0))
        );
        assert_eq!(limited(1.0, -1.0, 0), format!("{},{limiter}", linear(1.0)));
        assert!(limited(1.0, -2.0, 44_100).contains("alimiter=limit=0.74989:"));
        assert_eq!((ceiling(true), ceiling(false)), (-1.0, -2.0));
        assert_eq!(measure(None, false), "ebur128=peak=true:framelog=quiet");
        assert_eq!(
            measure(Some("anull"), true),
            "anull,ebur128=peak=true:framelog=quiet:dualmono=true"
        );
    }
}
