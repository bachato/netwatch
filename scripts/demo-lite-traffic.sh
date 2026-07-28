#!/usr/bin/env bash
# Background traffic generator for the NetWatch Lite demo recording.
#
# Lite's talker table groups by (process, host), so a good demo needs several
# DISTINCT hosts transferring CONCURRENTLY. Two shapes that don't work:
#
#   - One host hammered repeatedly collapses to a single row.
#   - Short bursts (plain `curl host`) finish before the next connection
#     snapshot, so rows flicker in and out and mostly render "—".
#
# So each host gets one long-lived, rate-limited download: slow enough to stay
# in the connection table for the whole recording, fast enough to register on
# the 1 Hz throughput sampler. Rates are staggered so the rows sort into a
# stable, visibly-different order rather than all reading the same number.
#
# The HOST column shows the TLS SNI, which requires the handshake to be
# captured — so start this BEFORE netwatch and the first connections will
# already be established (their SNI missed), while the periodic restarts below
# give netwatch fresh handshakes to read.
#
# Usage:
#   ./scripts/demo-lite-traffic.sh     # runs until killed
#   pkill -f demo-lite-traffic.sh
set -uo pipefail

# url|rate — distinct hosts, each big enough to outlast the recording.
STREAMS=(
  "https://speed.cloudflare.com/__down?bytes=200000000|900k"
  "https://ftp.gnu.org/gnu/bash/bash-5.2.tar.gz|120k"
  "https://download.thinkbroadband.com/100MB.zip|300k"
  "https://archive.org/download/nasa_techdocs/nasa_techdocs_meta.xml|40k"
)

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  pkill -P $$ curl 2>/dev/null || true
  exit 0
}
trap cleanup INT TERM EXIT

for spec in "${STREAMS[@]}"; do
  url="${spec%%|*}"
  rate="${spec##*|}"
  (
    while :; do
      # --limit-rate keeps the flow alive; the restart gives netwatch a fresh
      # ClientHello to read the SNI from partway through a recording.
      curl -s --limit-rate "$rate" --max-time 30 -o /dev/null "$url" || true
      sleep 2
    done
  ) &
  pids+=($!)
done

wait
