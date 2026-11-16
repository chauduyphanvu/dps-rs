#!/bin/sh
# Times `tonewell process --preset music` end to end on two generated WAV
# files: a 4-minute 44.1 kHz 16-bit track and a 60-minute 48 kHz 24-bit set
# (1 GB), both stereo pink noise with a 55 Hz tone, a 2 Hz tremolo, a quiet
# 20 s in every minute and 2 s of silence at each end. ffmpeg makes them from
# fixed seeds, so every run sees the same input.
#
#   scripts/bench-process.sh [BINARY ...]
#
# With no BINARY it builds and times target/release/tonewell. Give two to
# compare builds: their runs alternate, so drift and background load land
# on both. Prints the median wall and CPU time of RUNS runs (default 5).
set -eu

die() {
    echo "bench-process: $*" >&2
    exit 1
}

cd "$(dirname "$0")/.."
command -v ffmpeg >/dev/null || die "needs ffmpeg"
runs=${RUNS:-5}
dir=target/bench-process
mkdir -p "$dir"

if [ $# -eq 0 ]; then
    cargo build --release --locked
    set -- target/release/tonewell
fi

workload() {
    name=$1 seconds=$2 rate=$3 codec=$4 seed=$5
    [ -f "$dir/$name.wav" ] && return
    echo "generating $dir/$name.wav" >&2
    body=$((seconds - 4))
    noise="anoisesrc=d=$body:c=pink:r=$rate:a=0.3"
    ffmpeg -nostdin -hide_banner -loglevel error -y \
        -f lavfi -i "$noise:s=$seed" -f lavfi -i "$noise:s=$((seed + 1))" \
        -f lavfi -i "sine=f=55:r=$rate:d=$body" \
        -filter_complex "[0][2]amix=inputs=2:normalize=0[l];[1][2]amix=inputs=2:normalize=0[r];\
[l][r]join=inputs=2:channel_layout=stereo,tremolo=f=2:d=0.7,\
volume='if(lt(mod(t,60),20),0.2,1)':eval=frame,adelay=delays=2000:all=1,apad=pad_dur=2" \
        -c:a "$codec" "$dir/$name.wav"
}
workload typical 240 44100 pcm_s16le 1
workload large 3600 48000 pcm_s24le 3

median() {
    sort -n | awk '{v[NR] = $1} END {print (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2}'
}

for name in typical large; do
    for _ in $(seq "$runs"); do
        n=0
        for binary in "$@"; do
            n=$((n + 1))
            /usr/bin/time -p "$binary" process --preset music "$dir/$name.wav" "$dir/out.wav" \
                2>>"$dir/times.$n" >/dev/null || die "$binary failed on $name.wav"
        done
    done
    n=0
    for binary in "$@"; do
        n=$((n + 1))
        wall=$(awk '/^real/ {print $2}' "$dir/times.$n" | median)
        cpu=$(awk '/^user/ {u[++i] = $2} /^sys/ {s[++j] = $2} END {for (k = 1; k <= i; k++) print u[k] + s[k]}' \
            "$dir/times.$n" | median)
        printf '%-8s %-40s wall %6.2f s  cpu %6.2f s  (median of %s)\n' "$name" "$binary" "$wall" "$cpu" "$runs"
        rm -f "$dir/times.$n"
    done
done
rm -f "$dir/out.wav"
