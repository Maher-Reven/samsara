#!/usr/bin/env bash
# Regenerate ../../assets/sweep.svg from a real sweep.
set -euo pipefail
cd "$(dirname "$0")"
SAMSARA=${SAMSARA:-../../target/release/samsara}

node provider.mjs >/dev/null 2>&1 &
PROVIDER=$!
trap 'kill $PROVIDER 2>/dev/null || true' EXIT
sleep 1

if [ ! -f run.samsara.jsonl ]; then
  SAMSARA_UPSTREAM=http://127.0.0.1:8899 "$SAMSARA" record \
    --out run.samsara.jsonl --port 8911 -- node agent.mjs >/dev/null 2>&1
fi

# Strip the progress line: it is a carriage-return animation and means
# nothing in a still image.
"$SAMSARA" sweep run.samsara.jsonl --port 8912 -- node agent.mjs 2>/dev/null \
  | grep -v "replays of" \
  | sed -e 's/\r.*\r//' \
  | python3 ../../tools/ansi2svg.py > ../../assets/sweep.svg || true

echo "wrote assets/sweep.svg"
