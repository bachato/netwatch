#!/usr/bin/env bash
# Background traffic for the NetWatch egress-policy demo recording.
#
# The demo has to tell a three-beat story, and each beat has a hard
# requirement the traffic generator has to satisfy:
#
#   1. LEARN    — one process reaching a small, stable set of named
#                 destinations, long enough for the Egress tab to build a
#                 profile worth promoting.
#   2. PROMOTE  — nothing new must appear while the operator is pressing
#                 Enter, or the promoted rule captures a destination the
#                 viewer never saw arrive.
#   3. DRIFT    — the *same* process reaches somewhere it has never been,
#                 and only after the rule exists. That is the whole point:
#                 drift is a statement about a program that already had a
#                 baseline.
#
# So this runs in two phases with a gap between them, rather than as one
# continuous generator.
#
# Destination names come from the TLS ClientHello, which means the handshake
# has to be captured — a connection established before netwatch starts
# capturing shows as an IP forever. Every request below is therefore issued
# after launch, and the long-lived ones are restarted periodically so there
# is always a fresh handshake to read.
#
# Usage:
#   ./scripts/demo-egress-traffic.sh baseline   # phase 1, runs until killed
#   ./scripts/demo-egress-traffic.sh drift      # phase 3, one new destination
#   pkill -f demo-egress-traffic.sh
set -uo pipefail

# The baseline: two named hosts, rate-limited so the flows stay in the
# connection table across several snapshots instead of flickering. Distinct
# hosts, because a profile with one destination does not look like a profile.
# Both must be LARGE files. The first take used `api.github.com/zen`, which
# returns one line: it completed instantly whatever the rate limit said, never
# appeared in a connection snapshot, and curl learned exactly one destination —
# which does not look like a baseline worth promoting.
BASELINE=(
  "https://ftp.gnu.org/gnu/bash/bash-5.2.tar.gz|60k"
  "https://download.thinkbroadband.com/100MB.zip|80k"
)

# The drift destination. Deliberately somewhere plausible-but-different: the
# story is "a program you know started talking somewhere new", not "malware".
DRIFT_URL="https://speed.cloudflare.com/__down?bytes=20000000"
DRIFT_RATE="80k"

phase_baseline() {
  while true; do
    for stream in "${BASELINE[@]}"; do
      url="${stream%%|*}"
      rate="${stream##*|}"
      # --limit-rate keeps the flow alive; --max-time bounds it so the loop
      # comes back around and issues a fresh handshake for netwatch to read.
      curl -s --limit-rate "$rate" --max-time 20 -o /dev/null "$url" &
    done
    wait
  done
}

phase_drift() {
  curl -s --limit-rate "$DRIFT_RATE" --max-time 30 -o /dev/null "$DRIFT_URL"
}

case "${1:-baseline}" in
  baseline) phase_baseline ;;
  drift)    phase_drift ;;
  *)        echo "usage: $0 [baseline|drift]" >&2; exit 2 ;;
esac
