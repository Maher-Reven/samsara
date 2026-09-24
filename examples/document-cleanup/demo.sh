#!/usr/bin/env bash
# The whole walkthrough, non-interactively. See README.md for the narration.
set -euo pipefail
cd "$(dirname "$0")"

SAMSARA=${SAMSARA:-../../target/release/samsara}
[ -x "$SAMSARA" ] || { echo "build first: cargo build --release" >&2; exit 1; }
[ -d node_modules ] || { echo "install first: npm install" >&2; exit 1; }

node provider.mjs >/dev/null 2>&1 &
PROVIDER=$!
trap 'kill $PROVIDER 2>/dev/null || true' EXIT
sleep 1

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "1. The agent works"
rm -f ledger.log
node agent.mjs
cat ledger.log

say "2. Record one run"
rm -f run.samsara.jsonl; rm -rf run.samsara.objects
SAMSARA_UPSTREAM=http://127.0.0.1:8899 "$SAMSARA" record --out run.samsara.jsonl -- node agent.mjs
"$SAMSARA" show run.samsara.jsonl

say "3. Replay it — the world is not touched"
rm -f ledger.log
"$SAMSARA" replay run.samsara.jsonl --strict -- node agent.mjs
echo "ledger lines after replay: $(wc -l < ledger.log 2>/dev/null || echo 0)"

say "4. Break it, every way it can be broken"
"$SAMSARA" sweep run.samsara.jsonl --out cert.json -- node agent.mjs || true

say "5. The same agent, fixed"
rm -f fixed.samsara.jsonl; rm -rf fixed.samsara.objects
FIXED=1 SAMSARA_UPSTREAM=http://127.0.0.1:8899 "$SAMSARA" record --out fixed.samsara.jsonl -- node agent.mjs
FIXED=1 "$SAMSARA" sweep fixed.samsara.jsonl --out samsara.cert.json -- node agent.mjs

say "6. The certificate holds"
FIXED=1 "$SAMSARA" sweep fixed.samsara.jsonl --check samsara.cert.json -- node agent.mjs
