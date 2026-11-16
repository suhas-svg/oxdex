#!/usr/bin/env bash
# Full English Wikipedia, end to end: every number in the spec's performance
# table. Run from the repo root after the dump and its index are downloaded:
#
#     scripts/full-english.sh 2>&1 | tee data/out/enwiki-run.log
#
# Steps: ingest -> index -> check -> relevance eval -> in-process bench ->
# oha load test against the live service -> tantivy comparison.
# Stops at the first failing step.
set -euo pipefail

DUMP=${DUMP:-data/dumps/enwiki-20261101-pages-articles-multistream.xml.bz2}
STORE=${STORE:-data/out/enwiki}
INDEX=${INDEX:-data/index/enwiki}
TANTIVY=${TANTIVY:-data/tantivy/enwiki}
MEM_MB=${MEM_MB:-1024}
PORT=${PORT:-8080}
BIN=target/release

step() { printf '\n===== %s  %s =====\n' "$(date '+%H:%M:%S')" "$*"; }
timed() { local t0=$SECONDS; "$@"; echo "[wall] $(( SECONDS - t0 )) s: $*"; }

step "build release binaries"
cargo build --release -p oxdex-ingest -p oxdex-indexer -p oxdex-search-api -p oxdex-tantivy-bench

step "ingest $DUMP -> $STORE"
timed "$BIN/oxdex-ingest" build --dump "$DUMP" --out "$STORE" --force
du -sb "$STORE"

step "index $STORE -> $INDEX (buffer $MEM_MB MB)"
timed "$BIN/oxdex-indexer" build --store "$STORE" --out "$INDEX" --mem-mb "$MEM_MB" --force
du -sb "$INDEX" "$INDEX/title"

step "check index"
timed "$BIN/oxdex-indexer" check --index "$INDEX"

step "relevance: 50-query eval set"
"$BIN/oxdex-search-api" --index "$INDEX" --store "$STORE" eval || echo "eval failed (see above); continuing"

# 1,000 random redirect titles from English Wikipedia, the same recipe as
# benches/queries-simple.txt, plus the 40 broad queries.
step "build English query set"
cut -f1 "$STORE/redirects.tsv" | grep -v ':' | shuf --random-source=<(yes 42) -n 1000 > benches/queries-enwiki.txt
python -c "
import urllib.parse, sys
qs = [l.strip() for l in open('benches/queries-enwiki.txt', encoding='utf-8') if l.strip()]
open('benches/urls-enwiki.txt', 'w', encoding='utf-8', newline='\n').write(
    ''.join('http://127.0.0.1:$PORT/search?q=' + urllib.parse.quote_plus(q) + '\n' for q in qs))
"

step "in-process latency (round 1 cold, later rounds warm)"
"$BIN/oxdex-search-api" --index "$INDEX" --store "$STORE" bench --queries benches/queries-enwiki.txt --rounds 3
"$BIN/oxdex-search-api" --index "$INDEX" --store "$STORE" bench --queries benches/queries-broad.txt --rounds 3

step "startup + oha load test"
t0=$(date +%s%N)
"$BIN/oxdex-search-api" --index "$INDEX" --store "$STORE" --addr "127.0.0.1:$PORT" > "$STORE/../enwiki-api.log" 2>&1 &
API=$!
trap 'kill $API 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/health" > /dev/null; do :; done
echo "startup to /health OK: $(( ($(date +%s%N) - t0) / 1000000 )) ms"
oha --no-tui --urls-from-file -n 2000 -c 8 benches/urls-enwiki.txt --output-format quiet   # warm up
for c in 1 12 50; do
  echo "--- oha, $c clients, 30 s"
  oha --no-tui --urls-from-file -z 30s -c "$c" benches/urls-enwiki.txt
done
curl -s "127.0.0.1:$PORT/stats"; echo
kill $API; trap - EXIT

step "tantivy comparison"
timed "$BIN/oxdex-tantivy-bench" build --store "$STORE" --out "$TANTIVY" --mem-mb "$MEM_MB"
"$BIN/oxdex-tantivy-bench" eval --index "$TANTIVY" --file benches/eval-simple.tsv || echo "tantivy eval failed; continuing"
"$BIN/oxdex-tantivy-bench" bench --index "$TANTIVY" --queries benches/queries-enwiki.txt
"$BIN/oxdex-tantivy-bench" bench --index "$TANTIVY" --queries benches/queries-broad.txt
"$BIN/oxdex-search-api" --index "$INDEX" --store "$STORE" bench --queries benches/queries-enwiki.txt --rounds 2

step "done"
