#!/usr/bin/env bash
# Generates the synthetic role-played call (fixtures/roleplay/call.txt, written for this
# repository) as audio: line by line, speaker A in one espeak-ng voice and speaker B in
# another, with a two-second gap between turns and the script's [pause N] silences. No real
# person's voice. Writes, into a directory (default target/roleplay):
#
#   call.wav       16-bit mono PCM, espeak-ng's 22,050 Hz
#   timings.json   every line: speaker (0 = A, 1 = B), start and end in ms, text
#
# The meetings test a_thirty_minute_role_played_call_becomes_a_speaker_labelled_transcript
# reads both (MEETCRM_ROLEPLAY_DIR). The audio is about 80 MB: it is never committed; CI
# generates it. Needs espeak-ng (CI installs the Ubuntu package at a pinned version) and
# python3.
#
#   scripts/make-roleplay.sh [directory]
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
case "${1:-}" in -*) echo "make-roleplay.sh: unknown option $1; the only argument is a directory (see --help)" >&2; exit 2 ;; esac
[ $# -le 1 ] || { echo "make-roleplay.sh: at most one argument (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."
command -v espeak-ng >/dev/null || { echo "error: espeak-ng is not installed" >&2; exit 1; }
out="${1:-target/roleplay}"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Two distinct voices: an American male and a British female variant, at a steady pace.
voice_a="en-us+m3"
voice_b="en-gb+f3"
n=0
while IFS= read -r line; do
  case "$line" in
    "#"*|"") continue ;;
    "[pause "*"]") n=$((n + 1)); printf 'pause %s\n' "${line//[^0-9]/}" >"$work/$n.cue" ;;
    "A: "*|"B: "*)
      n=$((n + 1))
      who="${line%%:*}"; text="${line#?: }"
      voice="$voice_a"; [ "$who" = B ] && voice="$voice_b"
      espeak-ng -v "$voice" -s 115 -w "$work/$n.wav" -- "$text"
      printf '%s\n%s\n' "$who" "$text" >"$work/$n.cue" ;;
    *) echo "error: unrecognised script line: $line" >&2; exit 1 ;;
  esac
done <fixtures/roleplay/call.txt

python3 -I - "$work" "$n" "$out" <<'PY'
import json, sys, wave
work, count, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
GAP_MS = 2000
rate = None
frames = bytearray()
timings = []
def silence(ms):
    frames.extend(b"\x00\x00" * (rate * ms // 1000))
for i in range(1, count + 1):
    cue = open(f"{work}/{i}.cue").read().split("\n")
    if cue[0].startswith("pause "):
        silence(int(cue[0].split()[1]) * 1000)
        continue
    with wave.open(f"{work}/{i}.wav") as w:
        if rate is None:
            rate = w.getframerate()
        assert w.getframerate() == rate and w.getnchannels() == 1 and w.getsampwidth() == 2
        pcm = w.readframes(w.getnframes())
    start = len(frames) // 2 * 1000 // rate
    frames.extend(pcm)
    end = len(frames) // 2 * 1000 // rate
    timings.append({"speaker": 0 if cue[0] == "A" else 1, "start_ms": start, "end_ms": end, "text": cue[1]})
    silence(GAP_MS)
with wave.open(f"{out}/call.wav", "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate); w.writeframes(bytes(frames))
json.dump(timings, open(f"{out}/timings.json", "w"), indent=1)
total = len(frames) // 2 / rate
if not 29 * 60 <= total <= 31 * 60:
    sys.exit(f"error: the call is {total / 60:.1f} minutes, not about thirty")
print(f"roleplay ok: {len(timings)} lines, {total / 60:.1f} minutes, in {out}")
PY
