//! The transcript importer: a provider's answer (Deepgram's pre-recorded JSON with
//! `utterances=true&diarize=true`) to [`Utterance`]s. Everything in it is untrusted:
//! sizes, counts, times and speaker numbers are bounded, times are parsed as decimals
//! (never floats), text loses its control characters and is cut at a bound, and an answer
//! whose own duration falls short of the audio sent is partial, never complete.

use serde::Deserialize;

use crate::{TranscribeError, Utterance};

/// The largest answer read.
pub const MAX_ANSWER_BYTES: usize = 16 * 1024 * 1024;
/// The most utterances one chunk may have.
pub const MAX_UTTERANCES: usize = 50_000;
/// The most characters one utterance keeps.
pub const MAX_TEXT_CHARS: usize = 4_000;
/// The most speakers one chunk may have.
pub const MAX_SPEAKERS: u32 = 32;
/// How far an answer's times may run past the audio's measured end, or its duration fall
/// short of it, before it is refused (container rounding).
pub const TOLERANCE_MS: u64 = 1_000;

#[derive(Deserialize)]
struct Answer {
    metadata: Option<Metadata>,
    results: Option<Results>,
}

#[derive(Deserialize)]
struct Metadata {
    duration: Option<serde_json::Number>,
}

#[derive(Deserialize)]
struct Results {
    utterances: Option<Vec<Raw>>,
}

#[derive(Deserialize)]
struct Raw {
    start: serde_json::Number,
    end: serde_json::Number,
    speaker: Option<u32>,
    transcript: String,
}

/// Imports an answer for a chunk of `duration_ms`.
pub fn deepgram(body: &[u8], duration_ms: u64) -> Result<Vec<Utterance>, TranscribeError> {
    // LIMIT(answer-bytes)
    if body.len() > MAX_ANSWER_BYTES {
        return Err(TranscribeError::Malformed);
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|_| TranscribeError::Malformed)?;
    let covered = answer
        .metadata
        .and_then(|m| m.duration)
        .and_then(|d| seconds_ms(&d))
        .ok_or(TranscribeError::Malformed)?;
    if covered.saturating_add(TOLERANCE_MS) < duration_ms {
        return Err(TranscribeError::Partial);
    }
    let raw = answer
        .results
        .and_then(|r| r.utterances)
        .ok_or(TranscribeError::Malformed)?;
    // LIMIT(utterances)
    if raw.len() > MAX_UTTERANCES {
        return Err(TranscribeError::Malformed);
    }
    let limit = duration_ms.saturating_add(TOLERANCE_MS);
    let mut out = Vec::with_capacity(raw.len());
    for u in raw {
        let start_ms = seconds_ms(&u.start).ok_or(TranscribeError::Malformed)?;
        let end_ms = seconds_ms(&u.end).ok_or(TranscribeError::Malformed)?;
        let speaker = u.speaker.ok_or(TranscribeError::Malformed)?;
        if start_ms > end_ms || end_ms > limit || speaker >= MAX_SPEAKERS {
            return Err(TranscribeError::Malformed);
        }
        let text = clean(&u.transcript);
        if text.is_empty() {
            continue;
        }
        out.push(Utterance {
            speaker,
            start_ms,
            end_ms,
            text,
        });
    }
    out.sort_by_key(|u| (u.start_ms, u.end_ms));
    Ok(out)
}

/// A JSON number of seconds (`12`, `0.08`, `1800.0239`) as whole milliseconds; negative,
/// exponent or absurd values are `None`.
pub fn seconds_ms(n: &serde_json::Number) -> Option<u64> {
    let text = n.to_string();
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    if whole.is_empty()
        || whole.len() > 9
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
    Some(whole * 1000 + ms)
}

/// Text without control characters (newlines become spaces), whitespace collapsed, at
/// most [`MAX_TEXT_CHARS`] characters.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_TEXT_CHARS * 4));
    let mut chars = 0usize;
    let mut space = false;
    for c in text.chars() {
        // LIMIT(utterance-text)
        if chars >= MAX_TEXT_CHARS {
            break;
        }
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        // Bidirectional overrides and zero-width characters can disguise text.
        if matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        {
            continue;
        }
        if space {
            out.push(' ');
            chars += 1;
            space = false;
        }
        out.push(c);
        chars += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn answer(duration: &serde_json::Value, utterances: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "metadata": {"request_id": "r", "duration": duration, "channels": 1},
            "results": {"channels": [], "utterances": utterances}
        }))
        .unwrap()
    }

    #[test]
    fn a_diarised_answer_imports_in_time_order() {
        let body = answer(
            &json!(10.02),
            &json!([
                {"start": 4.5, "end": 9.9, "speaker": 1, "transcript": "Second\nline.", "confidence": 0.9},
                {"start": 0.08, "end": 4.2, "speaker": 0, "transcript": "  First   line. "}
            ]),
        );
        let got = deepgram(&body, 10_000).unwrap();
        assert_eq!(
            got,
            vec![
                Utterance {
                    speaker: 0,
                    start_ms: 80,
                    end_ms: 4_200,
                    text: "First line.".into()
                },
                Utterance {
                    speaker: 1,
                    start_ms: 4_500,
                    end_ms: 9_900,
                    text: "Second line.".into()
                },
            ]
        );
    }

    #[test]
    fn a_partial_answer_is_never_complete() {
        let body = answer(
            &json!(4.0),
            &json!([{"start": 0, "end": 4, "speaker": 0, "transcript": "Hi"}]),
        );
        assert_eq!(deepgram(&body, 10_000), Err(TranscribeError::Partial));
        assert!(deepgram(&body, 5_000).is_ok());
    }

    #[test]
    fn answers_that_break_a_bound_are_malformed() {
        let u = |s: serde_json::Value, e: serde_json::Value, sp: serde_json::Value| {
            answer(
                &json!(10),
                &json!([{"start": s, "end": e, "speaker": sp, "transcript": "x"}]),
            )
        };
        let cases = [
            u(json!(5), json!(4), json!(0)),
            u(json!(0), json!(11.5), json!(0)),
            u(json!(-1), json!(1), json!(0)),
            u(json!(0), json!(1e3), json!(0)),
            u(json!(0), json!(1), json!(32)),
            u(json!(0), json!(1), json!(null)),
            u(json!("0"), json!(1), json!(0)),
            br#"{"metadata":{"duration":10},"results":{}}"#.to_vec(),
            br#"{"results":{"utterances":[]}}"#.to_vec(),
            b"not json".to_vec(),
        ];
        for body in cases {
            assert_eq!(
                deepgram(&body, 10_000),
                Err(TranscribeError::Malformed),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
        let one = json!([{"start": 0, "end": 1, "speaker": 0, "transcript": "x"}]);
        let mut padded = answer(&json!(10), &one);
        assert!(deepgram(&padded, 10_000).is_ok());
        padded.resize(MAX_ANSWER_BYTES + 1, b' ');
        assert_eq!(deepgram(&padded, 10_000), Err(TranscribeError::Malformed));
        let many: Vec<_> = (0..=MAX_UTTERANCES)
            .map(|_| json!({"start": 0, "end": 1, "speaker": 0, "transcript": "x"}))
            .collect();
        assert_eq!(
            deepgram(&answer(&json!(10), &json!(many)), 10_000),
            Err(TranscribeError::Malformed)
        );
    }

    #[test]
    fn text_is_cleaned_and_bounded() {
        assert_eq!(clean("a\u{202E}b\u{0007}c\r\nd"), "ab c d");
        assert_eq!(
            clean(&"é".repeat(MAX_TEXT_CHARS + 10)).chars().count(),
            MAX_TEXT_CHARS
        );
        assert_eq!(clean(" \n\t "), "");
    }

    proptest! {
        // Fuzz target: the transcript importer, on arbitrary bytes.
        #[test]
        fn importing_arbitrary_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = deepgram(&bytes, 60_000);
        }

        // And on well-shaped answers with arbitrary values: whatever is accepted is bounded.
        #[test]
        fn imported_utterances_are_bounded(
            duration in 0u32..100_000,
            items in proptest::collection::vec((0u32..200_000, 0u32..200_000, 0u32..64, ".{0,64}"), 0..32),
        ) {
            let utterances: Vec<_> = items.iter().map(|(s, e, sp, t)| json!({
                "start": serde_json::Number::from_f64(f64::from(*s) / 1000.0),
                "end": serde_json::Number::from_f64(f64::from(*e) / 1000.0),
                "speaker": sp, "transcript": t
            })).collect();
            let body = answer(&json!(f64::from(duration) / 1000.0), &json!(utterances));
            if let Ok(got) = deepgram(&body, u64::from(duration)) {
                for u in got {
                    prop_assert!(u.start_ms <= u.end_ms && u.end_ms <= u64::from(duration) + TOLERANCE_MS);
                    prop_assert!(u.speaker < MAX_SPEAKERS);
                    prop_assert!(!u.text.is_empty() && u.text.chars().all(|c| !c.is_control()));
                }
            }
        }

        #[test]
        fn cleaning_is_idempotent(text in ".{0,256}") {
            let once = clean(&text);
            prop_assert_eq!(clean(&once), once.clone());
        }
    }
}
