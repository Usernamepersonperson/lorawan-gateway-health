#!/bin/sh
# Smoke + footprint test for a built binary pair on a Linux host or container (busybox is enough).
#   scripts/measure.sh DIR [RATE]   DIR holds lgh-exporter and lgh-replay; RATE = packets/s (default 4)
# Prints binary sizes, then VmRSS/VmHWM of the exporter after 20 s of replayed traffic and the scrape size.
set -eu
DIR=${1:?usage: measure.sh DIR [RATE]}
RATE=${2:-4}
EXP="$DIR/lgh-exporter"
REP="$DIR/lgh-replay"

echo "exporter size: $(wc -c < "$EXP") bytes"
echo "replay size:   $(wc -c < "$REP") bytes"

"$REP" bridge --listen 127.0.0.1:17000 --downlink-every 3 >/dev/null 2>&1 &
BR=$!
"$EXP" --tap-listen 127.0.0.1:17001 --upstream 127.0.0.1:17000 --metrics-listen 127.0.0.1:19089 >/dev/null 2>&1 &
EX=$!
sleep 1
"$REP" forwarder --target 127.0.0.1:17001 --rate "$RATE" --stat-interval 5 --duration 22 >/dev/null 2>&1 &
FW=$!
sleep 21

fetch() { wget -qO- http://127.0.0.1:19089/metrics; }
fetch >/dev/null
fetch >/tmp/lhe-scrape.txt
echo "scrape: $(wc -c < /tmp/lhe-scrape.txt) bytes, $(grep -vc '^#' /tmp/lhe-scrape.txt) samples"
grep -E '^lgh_(datagrams_total|bridge_up|forwarder_up|crc_error_ratio|tx_requests_total|exporter_resident_memory_bytes)' /tmp/lhe-scrape.txt
grep -E '^(VmRSS|VmHWM)' "/proc/$EX/status"

kill "$EX" "$BR" "$FW" 2>/dev/null || true
wait 2>/dev/null || true
