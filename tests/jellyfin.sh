#!/bin/sh
# Start a throwaway Jellyfin on 127.0.0.1:$PORT (8096 unless taken) with
# generated FLAC files: a "Sessions" album (three 44.1 kHz / 16-bit tracks,
# folder cover, lyrics for tracks 2 and 3) and a "HiRes" album (two 192 kHz /
# 24-bit tracks). Complete the startup wizard (admin "root"), create a
# "Music" library, scan it, add a second, non-admin user "melomane" the
# plugin signs in as, then run the end-to-end test against it. No audio
# device is ever opened: streams are downloaded to files and read with
# ffprobe.
#   tests/jellyfin.sh [workdir]      (needs docker, ffmpeg, curl, jq, python3)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(dirname "$HERE")
W=${1:-$(mktemp -d)}
NAME=ricercar-jellyfin-test
PORT=8096
while ss -ltnH "sport = :$PORT" | grep -q .; do PORT=$((PORT + 1)); done
mkdir -p "$W/music/Ensemble/Sessions" "$W/music/Trio/HiRes" "$W/config" "$W/cache"
for i in 1 2 3; do
  ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((300 * i)):duration=20" \
    -ar 44100 -sample_fmt s16 -metadata title="Track $i" -metadata artist=Ensemble \
    -metadata album_artist=Ensemble -metadata album=Sessions -metadata date=2021 \
    -metadata track=$i -metadata genre=Jazz "$W/music/Ensemble/Sessions/0$i.flac"
done
for i in 1 2; do
  ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((500 * i)):duration=15" \
    -ar 192000 -sample_fmt s32 -bits_per_raw_sample 24 -metadata title="Hi $i" \
    -metadata artist=Trio -metadata album_artist=Trio -metadata album=HiRes \
    -metadata date=2024 -metadata track=$i "$W/music/Trio/HiRes/0$i.flac"
done
ffmpeg -loglevel error -y -f lavfi -i color=c=0x8a5a3c:s=300x300 -frames:v 1 \
  "$W/music/Ensemble/Sessions/folder.jpg"
# Lyrics next to the files: synced for track 2, plain for track 3.
printf '[ar:Ensemble]\n[00:01.50]First line\n[00:04.00]Second line\n[01:02.25]Third line\n' \
  >"$W/music/Ensemble/Sessions/02.lrc"
printf 'Plain one\nPlain two\n' >"$W/music/Ensemble/Sessions/03.txt"

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:8096" --user "$(id -u):$(id -g)" \
  -e TZ=UTC -v "$W/music:/music:ro" -v "$W/config:/config" -v "$W/cache:/cache" \
  jellyfin/jellyfin:latest >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT INT TERM
JF="http://127.0.0.1:$PORT"
echo "jellyfin on $JF, workdir $W"

# Wait for the server (503 while it starts).
for _ in $(seq 120); do
  curl -sf "$JF/System/Info/Public" 2>/dev/null | grep -q '"Version"' && break
  sleep 1
done
J='Content-Type: application/json'
AH='MediaBrowser Client="e2e", Device="e2e", DeviceId="e2e-setup", Version="1"'
# Startup wizard.
curl -sf "$JF/Startup/Configuration" >/dev/null
curl -sf -X POST -H "$J" "$JF/Startup/Configuration" \
  -d '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}'
curl -sf "$JF/Startup/User" >/dev/null
curl -sf -X POST -H "$J" "$JF/Startup/User" -d '{"Name":"root","Password":"root-secret-9"}'
curl -sf -X POST -H "$J" "$JF/Startup/RemoteAccess" \
  -d '{"EnableRemoteAccess":true,"EnableAutomaticPortMapping":false}' || true
curl -sf -X POST "$JF/Startup/Complete"
for _ in $(seq 30); do
  curl -sf "$JF/System/Info/Public" | grep -q '"StartupWizardCompleted":true' && break
  sleep 1
done
ADMIN=$(curl -sf -X POST -H "$J" -H "Authorization: $AH" "$JF/Users/AuthenticateByName" \
  -d '{"Username":"root","Pw":"root-secret-9"}' | jq -r .AccessToken)
A="Authorization: $AH, Token=\"$ADMIN\""
# Music library, no internet metadata.
NOFETCH='{"Type":"MusicAlbum","MetadataFetchers":[],"ImageFetchers":[]},{"Type":"MusicArtist","MetadataFetchers":[],"ImageFetchers":[]},{"Type":"Audio","MetadataFetchers":[],"ImageFetchers":[]}'
curl -sf -X POST -H "$J" -H "$A" \
  "$JF/Library/VirtualFolders?name=Music&collectionType=music&refreshLibrary=false" \
  -d "{\"LibraryOptions\":{\"EnableRealtimeMonitor\":false,\"PathInfos\":[{\"Path\":\"/music\"}],\"TypeOptions\":[$NOFETCH]}}"
curl -sf -X POST -H "$A" "$JF/Library/Refresh"
n=0
for _ in $(seq 120); do
  sleep 1
  n=$(curl -sf -H "$A" "$JF/Items?Recursive=true&IncludeItemTypes=Audio" | jq .TotalRecordCount)
  a=$(curl -sf -H "$A" "$JF/Items?Recursive=true&IncludeItemTypes=MusicAlbum" | jq .TotalRecordCount)
  [ "$n" = 5 ] && [ "$a" = 2 ] && break
done
echo "scanned: $n tracks"
# Let the scan finish (album art, artists) before testing.
for _ in $(seq 60); do
  curl -sf -H "$A" "$JF/ScheduledTasks?isHidden=false" |
    jq -e '[.[] | select(.State != "Idle")] | length == 0' >/dev/null && break
  sleep 1
done
# The user the plugin signs in as.
curl -sf -X POST -H "$J" -H "$A" "$JF/Users/New" -d '{"Name":"melomane","Password":"sesame-42-pw"}' >/dev/null

(cd "$REPO" && cargo build --release)
# SETUP_ONLY=1: leave the server running for manual exploration.
if [ -n "${SETUP_ONLY:-}" ]; then trap - EXIT INT TERM; echo "ADMIN=$ADMIN"; exit 0; fi
set +e
JF="$JF" ADMIN="$ADMIN" BIN="$REPO/target/release/ricercar-jellyfin" python3 "$HERE/e2e.py" "$W"
rc=$?
docker logs "$NAME" >"$W/jellyfin.log" 2>&1
exit $rc
