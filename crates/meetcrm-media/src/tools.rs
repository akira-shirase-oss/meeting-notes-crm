//! ffprobe and ffmpeg, run with bounds. The program and every argument go to `execve` as a
//! vector: no shell sees them, and every path is one the store built from ids. Each run is
//! wrapped in `prlimit` (CPU seconds, largest file written, open files, no core dumps) with
//! a cleared environment and no stdin; a wall-clock deadline kills it; only the `file`
//! protocol and [`DEMUXERS`] are allowed, so a crafted container cannot open a socket or
//! a file it names.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::probe::{self, DEMUXERS, MAX_PROBE_OUTPUT, Probe};
use crate::{MediaError, MediaLimits};

const SIGXCPU: i32 = 24;
const SIGXFSZ: i32 = 25;
const SIGKILL: i32 = 9;

/// The sample rate chunks are transcoded to: what hosted speech models expect.
pub const CHUNK_SAMPLE_RATE: u32 = 16_000;

/// Where the tools are.
#[derive(Debug, Clone)]
pub struct Tools {
    /// The pinned ffmpeg.
    pub ffmpeg: PathBuf,
    /// The pinned ffprobe.
    pub ffprobe: PathBuf,
    /// util-linux `prlimit`, which sets the caps and then execs the tool.
    pub prlimit: PathBuf,
}

/// One transcription chunk on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Its position, from 0.
    pub index: u32,
    /// The file.
    pub path: PathBuf,
    /// Where it starts in the recording.
    pub offset_ms: u64,
    /// Its length.
    pub duration_ms: u64,
}

impl Tools {
    /// Probes `input` and refuses it if it must not be processed.
    pub async fn probe(&self, input: &Path, limits: &MediaLimits) -> Result<Probe, MediaError> {
        let mut args: Vec<OsString> = [
            "-v",
            "quiet",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            DEMUXERS,
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.push(file_url(input));
        let out = self
            .run(
                &self.ffprobe,
                &args,
                limits.probe_deadline,
                limits.cpu_seconds,
                0,
            )
            .await?;
        probe::parse(&out, limits.max_duration)
    }

    /// Transcodes the first audio stream of `input` to mono 16 kHz FLAC chunks of
    /// [`MediaLimits::chunk`] in `out_dir`, never reading past the duration cap, and
    /// measures each chunk.
    pub async fn transcode(
        &self,
        input: &Path,
        out_dir: &Path,
        limits: &MediaLimits,
    ) -> Result<Vec<Chunk>, MediaError> {
        let mut args: Vec<OsString> = Vec::with_capacity(40);
        let max_seconds = limits.max_duration.as_secs().to_string();
        let chunk_seconds = limits.chunk.as_secs().max(1).to_string();
        for a in [
            "-nostdin",
            "-hide_banner",
            "-v",
            "quiet",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            DEMUXERS,
            "-t",
            &max_seconds, // LIMIT(media-duration)
            "-i",
        ] {
            args.push(a.into());
        }
        args.push(file_url(input));
        for a in [
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "flac",
            "-f",
            "segment",
            "-segment_time",
            &chunk_seconds,
            "-reset_timestamps",
            "1",
            "-segment_list_type",
            "csv",
            "-segment_list",
        ] {
            args.push(a.into());
        }
        args.push(out_dir.join(SEGMENT_LIST).into_os_string());
        args.push(out_dir.join("chunk-%04d.flac").into_os_string());
        self.run(
            &self.ffmpeg,
            &args,
            limits.transcode_deadline,
            limits.cpu_seconds,
            limits.max_output_bytes,
        )
        .await?;

        let mut total = 0u64;
        let mut files = 0usize;
        let mut entries = tokio::fs::read_dir(out_dir)
            .await
            .map_err(|_| MediaError::Unavailable)?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|_| MediaError::Unavailable)?
        {
            let meta = entry
                .metadata()
                .await
                .map_err(|_| MediaError::Unavailable)?;
            total = total.saturating_add(meta.len());
            files += 1;
            if files > limits.max_chunks as usize + 1 {
                return Err(MediaError::OutputLimit); // LIMIT(chunk-count)
            }
        }
        if total > limits.max_output_bytes {
            return Err(MediaError::OutputLimit); // LIMIT(output-total)
        }
        let list = tokio::fs::read(out_dir.join(SEGMENT_LIST))
            .await
            .map_err(|_| MediaError::Corrupt)?;
        let chunks = segments(&list, limits.max_chunks)?;
        Ok(chunks
            .into_iter()
            .map(|(index, offset_ms, duration_ms)| Chunk {
                index,
                path: out_dir.join(chunk_name(index)),
                offset_ms,
                duration_ms,
            })
            .collect())
    }

    /// Runs `program` with `args` under the caps and returns its stdout. `fsize` 0 means
    /// the tool may write no file at all.
    async fn run(
        &self,
        program: &Path,
        args: &[OsString],
        deadline: Duration,
        cpu_seconds: u64,
        fsize: u64,
    ) -> Result<Vec<u8>, MediaError> {
        let mut command = Command::new(&self.prlimit);
        command
            .arg(format!("--cpu={}", cpu_seconds.max(1))) // LIMIT(ffmpeg-cpu)
            .arg(format!("--fsize={fsize}")) // LIMIT(ffmpeg-output)
            .arg("--nofile=64")
            .arg("--core=0")
            .arg("--")
            .arg(program)
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| MediaError::Unavailable)?;
        let mut stdout = child.stdout.take().ok_or(MediaError::Unavailable)?;
        let mut out = Vec::new();
        let waited = tokio::time::timeout(deadline, async {
            // LIMIT(probe-output)
            let limit = u64::try_from(MAX_PROBE_OUTPUT).unwrap_or(u64::MAX) + 1;
            (&mut stdout)
                .take(limit)
                .read_to_end(&mut out)
                .await
                .map_err(|_| MediaError::Unavailable)?;
            if out.len() > MAX_PROBE_OUTPUT {
                return Err(MediaError::Corrupt);
            }
            child.wait().await.map_err(|_| MediaError::Unavailable)
        })
        .await; // LIMIT(ffmpeg-deadline)
        let status = match waited {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(e);
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(MediaError::Deadline);
            }
        };
        match (status.code(), status.signal()) {
            (Some(0), _) => Ok(out),
            (_, Some(SIGXCPU | SIGKILL)) if status.code().is_none() => Err(MediaError::CpuLimit),
            (_, Some(SIGXFSZ)) => Err(MediaError::OutputLimit),
            // prlimit itself failing to exec the tool is our fault, not the file's.
            (Some(126 | 127), _) => Err(MediaError::Unavailable),
            _ => Err(MediaError::Corrupt),
        }
    }
}

/// The segment muxer's list of what it wrote.
const SEGMENT_LIST: &str = "segments.csv";

/// The file name of chunk `index`, as the segment muxer writes it.
pub fn chunk_name(index: u32) -> String {
    format!("chunk-{index:04}.flac")
}

/// The segment list (`chunk-0000.flac,0.000000,600.000000` per line) as
/// `(index, offset_ms, duration_ms)`: in order, contiguous, named as expected, at most
/// `max` of them, at least one.
pub fn segments(list: &[u8], max: u32) -> Result<Vec<(u32, u64, u64)>, MediaError> {
    let text = std::str::from_utf8(list).map_err(|_| MediaError::Corrupt)?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let mut out = Vec::new();
    for (index, line) in (0u32..).zip(lines.iter().copied()) {
        let last = usize::try_from(index).is_ok_and(|i| i + 1 == lines.len());
        let mut fields = line.split(',');
        let (Some(name), Some(start), Some(end), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(MediaError::Corrupt);
        };
        let start = probe::millis(start).ok_or(MediaError::Corrupt)?;
        let end = probe::millis(end).ok_or(MediaError::Corrupt)?;
        if name != chunk_name(index) || end < start {
            return Err(MediaError::Corrupt);
        }
        // The segment muxer closes with an empty segment when the input ends on a boundary.
        if end == start {
            if last && index > 0 {
                break;
            }
            return Err(MediaError::Corrupt);
        }
        if index >= max {
            return Err(MediaError::OutputLimit);
        }
        out.push((index, start, end - start));
    }
    if out.is_empty() {
        return Err(MediaError::Corrupt);
    }
    Ok(out)
}

/// `file:<path>`: without the protocol prefix a path with a colon would name a protocol.
fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    url
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    #[test]
    fn a_closing_empty_segment_is_dropped_and_any_other_is_corrupt() {
        let list = b"chunk-0000.flac,0.000000,2.016000\nchunk-0001.flac,2.016000,4.000000\nchunk-0002.flac,4.000000,4.000000\n";
        assert_eq!(segments(list, 2), Ok(vec![(0, 0, 2016), (1, 2016, 1984)]));
        let middle = b"chunk-0000.flac,0.0,1.0\nchunk-0001.flac,1.0,1.0\nchunk-0002.flac,1.0,2.0\n";
        assert_eq!(segments(middle, 9), Err(MediaError::Corrupt));
        assert_eq!(
            segments(b"chunk-0000.flac,0.0,0.0\n", 9),
            Err(MediaError::Corrupt)
        );
        assert_eq!(
            segments(b"chunk-0000.flac,0.0,1.0\nchunk-0001.flac,1.0,2.0\n", 1),
            Err(MediaError::OutputLimit)
        );
    }
}
