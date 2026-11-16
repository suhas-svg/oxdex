#!/usr/bin/env bash
# Finishes downloading the English dump (resuming), checks its size, then runs
# scripts/full-english.sh. Meant to be started detached so it survives the
# terminal or editor closing:
#
#     powershell Start-Process -WindowStyle Hidden "C:\Program Files\Git\bin\bash.exe" \
#         -ArgumentList "-lc","'cd /e/Desktop/Oxdex && scripts/fetch-and-run-english.sh'"
#
# Progress: data/out/enwiki-run.log. Stop it: Stop-Process -Name bash, curl, oxdex-*.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p data/out
exec >> data/out/enwiki-run.log 2>&1

DUMP_DIR=data/dumps
F=enwiki-20261101-pages-articles-multistream.xml.bz2
WANT=27019419473
echo "===== $(date '+%F %H:%M:%S') fetch-and-run started (pid $$) ====="

n=0
while [ "$(stat -c %s "$DUMP_DIR/$F")" != "$WANT" ] && [ $n -lt 400 ]; do
  n=$((n + 1))
  if [ $((n % 2)) = 1 ]; then SRC="https://dumps.wikimedia.org/enwiki/20261101/$F"; else SRC="https://dumps.wikimedia.your.org/enwiki/20261101/$F"; fi
  before=$(stat -c %s "$DUMP_DIR/$F")
  curl -sS -f -L -C - --speed-limit 50000 --speed-time 60 -A "oxdex-dev" -o "$DUMP_DIR/$F" "$SRC" 2>&1 | tail -1
  after=$(stat -c %s "$DUMP_DIR/$F")
  echo "$(date +%H:%M) try $n via ${SRC%%/enwiki*}: $before -> $after"
  [ "$after" = "$WANT" ] && break
  sleep 20
done
if [ "$(stat -c %s "$DUMP_DIR/$F")" != "$WANT" ]; then
  echo "SIZE MISMATCH: $(stat -c %s "$DUMP_DIR/$F") of $WANT bytes; not running the pipeline"
  exit 1
fi
echo "SIZE OK: $WANT bytes"

scripts/full-english.sh
echo "===== $(date '+%F %H:%M:%S') fetch-and-run finished, exit $? ====="
