//! ffprobe's `-print_format json -show_format -show_streams` output, parsed without trust:
//! the bytes come from a tool that just read an attacker's file. Bounded input, only the
//! fields the pipeline uses, every number checked, and the container re-checked against
//! the allowed demuxers.

use std::time::Duration;

use serde::Deserialize;

use crate::MediaError;

/// The demuxers ffprobe and ffmpeg may use (`-format_whitelist`). No playlists (hls, dash),
/// no concat, no image or subtitle formats: those can name other files or URLs.
pub const DEMUXERS: &str = "wav,mp3,flac,ogg,mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,aac";

/// The most bytes of ffprobe output read.
pub const MAX_PROBE_OUTPUT: usize = 256 * 1024; // LIMIT(probe-output)

/// The most streams a recording may declare.
pub const MAX_STREAMS: usize = 16; // LIMIT(probe-streams)

/// What the pipeline needs to know about a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// Its length, rounded down to the millisecond.
    pub duration_ms: u64,
    /// The first audio stream's sample rate.
    pub sample_rate: u32,
    /// The first audio stream's channel count.
    pub channels: u32,
}

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Stream {
    codec_type: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
}

#[derive(Deserialize)]
struct Format {
    format_name: Option<String>,
    duration: Option<String>,
}

/// Parses ffprobe's JSON and refuses what the pipeline must not process.
pub fn parse(json: &[u8], max_duration: Duration) -> Result<Probe, MediaError> {
    if json.len() > MAX_PROBE_OUTPUT {
        return Err(MediaError::Corrupt);
    }
    let out: Output = serde_json::from_slice(json).map_err(|_| MediaError::Corrupt)?;
    if out.streams.len() > MAX_STREAMS {
        return Err(MediaError::Corrupt);
    }
    let format = out.format.ok_or(MediaError::Corrupt)?;
    let name = format.format_name.ok_or(MediaError::Corrupt)?;
    // ffprobe names a demuxer family with commas ("mov,mp4,m4a,3gp,3g2,mj2"): every part
    // must be allowed.
    if name.is_empty()
        || !name
            .split(',')
            .all(|part| DEMUXERS.split(',').any(|d| d == part))
    {
        return Err(MediaError::Corrupt);
    }
    let audio = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"))
        .ok_or(MediaError::NoAudio)?;
    let sample_rate = audio
        .sample_rate
        .as_deref()
        .and_then(|r| r.parse::<u32>().ok())
        .filter(|r| (1..=384_000).contains(r))
        .ok_or(MediaError::Corrupt)?;
    let channels = audio
        .channels
        .filter(|c| (1..=64).contains(c))
        .ok_or(MediaError::Corrupt)?;
    let duration_ms = format
        .duration
        .as_deref()
        .and_then(millis)
        .ok_or(MediaError::Corrupt)?;
    if duration_ms == 0 {
        return Err(MediaError::Corrupt);
    }
    if u128::from(duration_ms) > max_duration.as_millis() {
        return Err(MediaError::TooLong);
    }
    Ok(Probe {
        duration_ms,
        sample_rate,
        channels,
    })
}

/// A decimal seconds value ("1800.023583") as whole milliseconds, without floating point.
/// At most ten integer digits: anything longer is not a real duration.
pub fn millis(seconds: &str) -> Option<u64> {
    let (whole, frac) = seconds.split_once('.').unwrap_or((seconds, ""));
    if whole.is_empty()
        || whole.len() > 10
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: u64 = whole.parse().ok()?;
    let mut ms = 0u64;
    for (i, b) in frac.bytes().take(3).enumerate() {
        ms += u64::from(b - b'0') * [100, 10, 1][i];
    }
    whole.checked_mul(1000)?.checked_add(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn sample(format: &str, duration: &str, codec: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "streams": [{"codec_type": codec, "sample_rate": "16000", "channels": 1}],
            "format": {"format_name": format, "duration": duration, "size": "1"}
        }))
        .unwrap()
    }

    #[test]
    fn a_readable_recording_is_measured() {
        let p = parse(&sample("wav", "1800.0239", "audio"), HOUR).unwrap();
        assert_eq!(
            p,
            Probe {
                duration_ms: 1_800_023,
                sample_rate: 16_000,
                channels: 1
            }
        );
        let family = sample("mov,mp4,m4a,3gp,3g2,mj2", "12", "audio");
        assert_eq!(parse(&family, HOUR).unwrap().duration_ms, 12_000);
    }

    #[test]
    fn what_must_not_be_processed_is_refused_with_its_reason() {
        let cases: [(&[u8], MediaError); 9] = [
            (&sample("hls", "10", "audio"), MediaError::Corrupt),
            (&sample("concat", "10", "audio"), MediaError::Corrupt),
            (&sample("wav,hls", "10", "audio"), MediaError::Corrupt),
            (&sample("wav", "10", "video"), MediaError::NoAudio),
            (&sample("wav", "3600.001", "audio"), MediaError::TooLong),
            (&sample("wav", "0.0", "audio"), MediaError::Corrupt),
            (&sample("wav", "-1", "audio"), MediaError::Corrupt),
            (&sample("wav", "nan", "audio"), MediaError::Corrupt),
            (b"{\"streams\": []}", MediaError::Corrupt),
        ];
        for (json, want) in cases {
            assert_eq!(
                parse(json, HOUR),
                Err(want),
                "{}",
                String::from_utf8_lossy(json)
            );
        }
        assert_eq!(
            parse(&sample("wav", "3600", "audio"), HOUR)
                .unwrap()
                .duration_ms,
            3_600_000
        );
        let huge = vec![b' '; MAX_PROBE_OUTPUT + 1];
        assert_eq!(parse(&huge, HOUR), Err(MediaError::Corrupt));
        let streams: Vec<_> = (0..=MAX_STREAMS)
            .map(|_| serde_json::json!({"codec_type": "audio", "sample_rate": "8000", "channels": 1}))
            .collect();
        let many = serde_json::to_vec(&serde_json::json!({
            "streams": streams, "format": {"format_name": "wav", "duration": "1"}
        }))
        .unwrap();
        assert_eq!(parse(&many, HOUR), Err(MediaError::Corrupt));
    }

    #[test]
    fn durations_parse_exactly() {
        assert_eq!(millis("0.5"), Some(500));
        assert_eq!(millis("1800.023583"), Some(1_800_023));
        assert_eq!(millis("7"), Some(7000));
        for bad in ["", ".5", "1e3", "1.2.3", " 1", "99999999999", "inf", "+1"] {
            assert_eq!(millis(bad), None, "{bad}");
        }
    }

    proptest! {
        // Fuzz target: the media probe parser. Arbitrary bytes and arbitrary JSON never
        // panic, and anything accepted is within the bounds.
        #[test]
        fn probe_parsing_never_panics_and_accepts_only_bounded_values(
            bytes in proptest::collection::vec(any::<u8>(), 0..2048)
        ) {
            if let Ok(p) = parse(&bytes, HOUR) {
                prop_assert!(p.duration_ms > 0 && p.duration_ms <= 3_600_000);
            }
        }

        #[test]
        fn probe_parsing_of_structured_input_is_bounded(
            format in "[a-z0-9,]{0,24}",
            duration in "[0-9.e+-]{0,16}",
            rate in "[0-9-]{0,8}",
            channels in any::<u32>(),
            codec in prop_oneof![Just("audio"), Just("video"), Just("data")],
        ) {
            let json = serde_json::to_vec(&serde_json::json!({
                "streams": [{"codec_type": codec, "sample_rate": rate, "channels": channels}],
                "format": {"format_name": format, "duration": duration}
            })).unwrap();
            if let Ok(p) = parse(&json, HOUR) {
                prop_assert!(format.split(',').all(|f| DEMUXERS.split(',').any(|d| d == f)));
                prop_assert!(p.duration_ms <= 3_600_000);
                prop_assert!((1..=64).contains(&p.channels));
            }
        }

        #[test]
        fn millis_matches_the_decimal_value(whole in 0u64..10_000_000, frac in 0u32..1_000_000) {
            let text = format!("{whole}.{frac:06}");
            prop_assert_eq!(millis(&text), Some(whole * 1000 + u64::from(frac / 1000)));
        }
    }
}
