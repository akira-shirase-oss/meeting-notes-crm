//! The runner against the pinned ffmpeg (scripts/install-ffmpeg.sh), and against stand-in
//! tools that hang or spin, for the caps.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use meetcrm_media::tools::Tools;
use meetcrm_media::{MediaError, MediaLimits};
use uuid::Uuid;

fn bin_dir() -> PathBuf {
    std::env::var_os("MEETCRM_FFMPEG_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tools/bin"),
        PathBuf::from,
    )
}

fn tools() -> Tools {
    let dir = bin_dir();
    assert!(
        dir.join("ffmpeg").exists(),
        "the pinned ffmpeg is missing: run scripts/install-ffmpeg.sh"
    );
    Tools {
        ffmpeg: dir.join("ffmpeg"),
        ffprobe: dir.join("ffprobe"),
        prlimit: PathBuf::from("prlimit"),
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meetcrm-media-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tone recording made directly (not through the runner, which refuses lavfi).
fn tone(dir: &Path, name: &str, seconds: u32) -> PathBuf {
    let path = dir.join(name);
    let status = std::process::Command::new(bin_dir().join("ffmpeg"))
        .args(["-v", "quiet", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .arg("-f")
        .arg("wav")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

fn fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_recording_is_probed_and_transcoded_into_measured_chunks() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 25);
    let limits = MediaLimits {
        chunk: Duration::from_secs(10),
        ..MediaLimits::default()
    };
    let probe = tools().probe(&input, &limits).await.unwrap();
    assert_eq!(probe.duration_ms, 25_000);
    let out = dir.join("chunks");
    std::fs::create_dir(&out).unwrap();
    let chunks = tools().transcode(&input, &out, &limits).await.unwrap();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[1].offset_ms, chunks[0].duration_ms);
    let total: u64 = chunks.iter().map(|c| c.duration_ms).sum();
    assert!((24_900..=25_100).contains(&total), "{total}");
    for c in &chunks {
        assert!(c.duration_ms <= 10_100, "{c:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn corrupt_and_long_recordings_are_refused_with_their_reasons() {
    let dir = scratch();
    let junk = dir.join("junk");
    std::fs::write(&junk, b"RIFF\x00\x00not really a recording at all").unwrap();
    assert_eq!(
        tools().probe(&junk, &MediaLimits::default()).await,
        Err(MediaError::Corrupt)
    );
    let long = tone(&dir, "long.wav", 5);
    let short_cap = MediaLimits {
        max_duration: Duration::from_secs(4),
        ..MediaLimits::default()
    };
    assert_eq!(
        tools().probe(&long, &short_cap).await,
        Err(MediaError::TooLong)
    );
    // Even unprobed, the transcode reads no further than the cap.
    let out = dir.join("chunks");
    std::fs::create_dir(&out).unwrap();
    let chunks = tools().transcode(&long, &out, &short_cap).await.unwrap();
    let total: u64 = chunks.iter().map(|c| c.duration_ms).sum();
    assert!(total <= 4_100, "{total}");
    std::fs::remove_dir_all(dir).unwrap();
}

// Attack: a "recording" that is a playlist naming a URL and a local file. With the
// protocol and demuxer lists ffmpeg opens neither: nothing connects, nothing is read.
#[tokio::test]
async fn a_playlist_cannot_make_ffmpeg_fetch_a_url_or_read_another_file() {
    let dir = scratch();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let playlist = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nhttp://127.0.0.1:{port}/a.wav\n#EXTINF:10,\nfile:/etc/hostname\n#EXT-X-ENDLIST\n"
    );
    let concat = "ffconcat version 1.0\nfile '/etc/hostname'\n";
    for (name, body) in [
        ("original", playlist.as_str()),
        ("evil.m3u8", playlist.as_str()),
        ("list.ffconcat", concat),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            tools().probe(&path, &MediaLimits::default()).await,
            Err(MediaError::Corrupt),
            "{name}"
        );
        let out = dir.join(format!("out-{name}"));
        std::fs::create_dir(&out).unwrap();
        assert_eq!(
            tools()
                .transcode(&path, &out, &MediaLimits::default())
                .await,
            Err(MediaError::Corrupt),
            "{name}"
        );
    }
    assert!(
        listener.accept().is_err(),
        "ffmpeg connected to the playlist's URL"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

// Attack: a path full of shell syntax is one argument, never a command.
#[tokio::test]
async fn a_path_is_never_interpreted_by_a_shell() {
    let dir = scratch();
    let name = "a b;$(touch pwned) `touch pwned2`.wav";
    let input = tone(&dir, "plain.wav", 1);
    let odd = dir.join(name);
    std::fs::rename(input, &odd).unwrap();
    let probe = tools().probe(&odd, &MediaLimits::default()).await.unwrap();
    assert_eq!(probe.duration_ms, 1_000);
    for marker in ["pwned", "pwned2"] {
        assert!(!dir.join(marker).exists() && !Path::new(marker).exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_hanging_tool_is_killed_at_its_deadline() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 1);
    let hang = fake_tool(&dir, "ffprobe", "sleep 60");
    let tools = Tools {
        ffprobe: hang,
        ..tools()
    };
    let limits = MediaLimits {
        probe_deadline: Duration::from_millis(300),
        ..MediaLimits::default()
    };
    let started = Instant::now();
    assert_eq!(
        tools.probe(&input, &limits).await,
        Err(MediaError::Deadline)
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_spinning_tool_is_killed_at_its_cpu_cap() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 1);
    let spin = fake_tool(&dir, "ffprobe", "while :; do :; done");
    let tools = Tools {
        ffprobe: spin,
        ..tools()
    };
    let limits = MediaLimits {
        cpu_seconds: 1,
        probe_deadline: Duration::from_secs(60),
        ..MediaLimits::default()
    };
    assert_eq!(
        tools.probe(&input, &limits).await,
        Err(MediaError::CpuLimit)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_transcode_past_its_output_cap_is_killed() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 20);
    let out = dir.join("chunks");
    std::fs::create_dir(&out).unwrap();
    let limits = MediaLimits {
        max_output_bytes: 4096,
        ..MediaLimits::default()
    };
    assert_eq!(
        tools().transcode(&input, &out, &limits).await,
        Err(MediaError::OutputLimit)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn more_chunks_than_allowed_is_refused() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 6);
    let out = dir.join("chunks");
    std::fs::create_dir(&out).unwrap();
    let limits = MediaLimits {
        chunk: Duration::from_secs(1),
        max_chunks: 3,
        ..MediaLimits::default()
    };
    assert_eq!(
        tools().transcode(&input, &out, &limits).await,
        Err(MediaError::OutputLimit)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_missing_tool_is_our_fault_not_the_recordings() {
    let dir = scratch();
    let input = tone(&dir, "in.wav", 1);
    let tools = Tools {
        ffprobe: dir.join("absent"),
        ..tools()
    };
    assert_eq!(
        tools.probe(&input, &MediaLimits::default()).await,
        Err(MediaError::Unavailable)
    );
    std::fs::remove_dir_all(dir).unwrap();
}
