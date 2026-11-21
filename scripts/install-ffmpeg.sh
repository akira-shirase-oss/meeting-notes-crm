#!/usr/bin/env bash
# Installs the pinned static ffmpeg and ffprobe (FFmpeg 7.0.2, amd64, the johnvansickle.com
# static build) into a directory (default target/tools/bin, where the tests and the server's
# defaults look), after checking the archive's SHA-256 against the value recorded here. The
# media pipeline runs only these binaries; a system ffmpeg is never used.
#
#   scripts/install-ffmpeg.sh [directory]
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
case "${1:-}" in -*) echo "install-ffmpeg.sh: unknown option $1; the only argument is a directory (see --help)" >&2; exit 2 ;; esac
cd "$(dirname "$0")/.."

readonly VERSION="7.0.2"
readonly SHA="abda8d77ce8309141f83ab8edf0596834087c52467f6badf376a6a2a4c87cf67"
dest="${1:-target/tools/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) asset="ffmpeg-$VERSION-amd64-static.tar.xz" ;;
  *) echo "error: no pinned ffmpeg build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if [ -x "$dest/ffmpeg" ] && [ -x "$dest/ffprobe" ] \
  && "$dest/ffmpeg" -version 2>/dev/null | head -n 1 | grep -q "ffmpeg version $VERSION-static"; then
  echo "ffmpeg ok: $VERSION already in $dest"
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# The build moves from releases/ to old-releases/ when a newer one ships; the checksum, not
# the location, is what is trusted.
fetched=""
for base in https://johnvansickle.com/ffmpeg/releases https://johnvansickle.com/ffmpeg/old-releases; do
  if curl -sSfL --retry 3 -o "$work/$asset" "$base/$asset"; then fetched=1; break; fi
done
[ -n "$fetched" ] || { echo "error: could not download $asset" >&2; exit 1; }
echo "$SHA  $work/$asset" | sha256sum -c - >/dev/null
tar -xJf "$work/$asset" -C "$work"
mkdir -p "$dest"
install -m 0755 "$work/ffmpeg-$VERSION-amd64-static/ffmpeg" "$dest/ffmpeg"
install -m 0755 "$work/ffmpeg-$VERSION-amd64-static/ffprobe" "$dest/ffprobe"
echo "ffmpeg ok: $("$dest/ffmpeg" -version | head -n 1 | cut -d' ' -f1-3) in $dest"
