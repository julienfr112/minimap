#!/usr/bin/env bash
# Download every European country extract from Geofabrik into data/countries/.
#
# Per-country rather than europe-latest.osm.pbf (34.7 GB as one file) because
# country-sized extracts are what make the pipeline tractable: each one's node
# index fits in memory, and `load --regions` takes as many as you ask for.
#
#   ./fetch-europe.sh          # 3 concurrent transfers
#   JOBS=6 ./fetch-europe.sh   # faster, less polite
#
# Safe to re-run and safe to interrupt: complete files are skipped by comparing
# local size to the server's content-length, partial files resume with curl -C -.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="$ROOT/data/countries"
JOBS="${JOBS:-3}"
mkdir -p "$DEST"

# Which extracts, and which aggregates to leave out, is pipeline knowledge --
# see EUROPE_SKIP in pipeline/src/download.rs. This script only moves bytes.
if [ ! -s "$DEST/.urls" ]; then
    echo "listing European extracts ..."
    cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" \
        --bin minimap -- europe-urls > "$DEST/.urls" || {
            echo "could not list extracts (is cargo installed?)" >&2; exit 1; }
fi

echo "$(wc -l < "$DEST/.urls") extracts -> $DEST  (JOBS=$JOBS)"

fetch() {
    url="$1"; dest="$2"
    name="${url##*/}"
    remote=$(curl -sIL -o /dev/null -w '%header{content-length}' "$url" --max-time 60)
    local_size=0
    [ -f "$dest/$name" ] && local_size=$(stat -c%s "$dest/$name")
    # Sizes printed as integer MB: %f formatting breaks under locales that use a
    # comma decimal separator (fr_FR here).
    mb=$(( ${remote:-0} / 1000000 ))
    if [ -n "$remote" ] && [ "$local_size" = "$remote" ]; then
        printf 'skip     %-44s %6d MB\n' "$name" "$mb"
        return 0
    fi
    printf 'get      %-44s %6d MB\n' "$name" "$mb"
    if curl -sSL -C - --retry 5 --retry-delay 5 --retry-all-errors \
            --max-time 7200 --output-dir "$dest" -O "$url"; then
        printf 'done     %-44s\n' "$name"
    else
        printf 'FAILED   %-44s (re-run to resume)\n' "$name"
    fi
}
export -f fetch

xargs -a "$DEST/.urls" -P "$JOBS" -I{} bash -c 'fetch "$@"' _ {} "$DEST"

echo "--- summary ---"
du -sh "$DEST" 2>/dev/null
ls -1 "$DEST"/*.osm.pbf 2>/dev/null | wc -l | xargs printf '%s files present\n'
