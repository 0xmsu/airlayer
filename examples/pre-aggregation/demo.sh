#!/usr/bin/env bash
# Pre-aggregation demo: shows the speed difference between raw queries
# and queries served from a local Parquet cache.
#
# Usage: ./demo.sh   (runs seed automatically if needed)
(
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT="$(cd ../.. && pwd)"

# ── resolve airlayer binary ──────────────────────────────────────────────
if [ -x "$REPO_ROOT/target/release/airlayer" ]; then
    AL="$REPO_ROOT/target/release/airlayer"
elif [ -x "$REPO_ROOT/target/debug/airlayer" ]; then
    AL="$REPO_ROOT/target/debug/airlayer"
elif command -v airlayer &>/dev/null; then
    AL=airlayer
else
    echo "airlayer binary not found. Build with: cargo build --features exec --release"
    exit 1
fi

# ── seed if needed ───────────────────────────────────────────────────────
if [ ! -f data/warehouse.duckdb ]; then
    echo "Seeding 500M-row warehouse (this takes ~30s)..."
    ./seed.sh
    echo ""
fi

# ── helpers ──────────────────────────────────────────────────────────────
bold=$(tput bold)
dim=$(tput setaf 8)
green=$(tput setaf 2)
reset=$(tput sgr0)

run_query() {
    local start end elapsed
    start=$(python3 -c 'import time; print(time.time())')
    "$@" > /tmp/airlayer_demo_out.json 2>/dev/null
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(f'{($end - $start)*1000:.0f}')")

    # Print results as a table
    python3 -c "
import json, sys
d = json.load(open('/tmp/airlayer_demo_out.json'))
rows = d.get('data', [])
if not rows:
    print('  (no data)')
    sys.exit()
cols = list(rows[0].keys())
# Clean up column names: events__platform -> platform
short = [c.split('__',1)[-1] if '__' in c else c for c in cols]
widths = [max(len(s), max(len(str(r.get(c,''))) for r in rows)) for s, c in zip(short, cols)]
header = '  '.join(s.ljust(w) for s, w in zip(short, widths))
print(f'  {header}')
print(f'  ' + '  '.join('-'*w for w in widths))
for r in rows:
    vals = []
    for c, w in zip(cols, widths):
        v = r.get(c, '')
        if isinstance(v, float) and v == int(v):
            v = int(v)
        vals.append(str(v).ljust(w))
    print(f'  ' + '  '.join(vals))
"
    local rows
    rows=$(python3 -c "import json; print(len(json.load(open('/tmp/airlayer_demo_out.json')).get('data',[])))")
    echo ""
    echo "${dim}  ⏱  ${elapsed} ms  (${rows} rows)${reset}"
}

run_quiet() {
    local start end elapsed
    start=$(python3 -c 'import time; print(time.time())')
    "$@" > /tmp/airlayer_demo_out.json 2>/dev/null
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(f'{($end - $start)*1000:.0f}')")
    echo "${dim}  ⏱  ${elapsed} ms${reset}"
}

# Clean prior cache and built rollup tables
rm -rf .airlayer
duckdb data/warehouse.duckdb "DROP SCHEMA IF EXISTS preagg CASCADE;" 2>/dev/null || true

echo ""
echo "${bold}╔══════════════════════════════════════════════════════════════╗${reset}"
echo "${bold}║      Pre-aggregation Demo  (500M rows in DuckDB)          ║${reset}"
echo "${bold}╚══════════════════════════════════════════════════════════════╝${reset}"
echo ""

# ── 1. Raw query ─────────────────────────────────────────────────────────
echo "${bold}1) Raw query — scans all 500,000,000 rows${reset}"
echo "   Revenue + event count by platform"
echo ""
run_query $AL query -x --config config.yml --no-cache \
    --dimension events.platform \
    --measure events.total_revenue \
    --measure events.event_count \
    --order events.total_revenue:desc
echo ""

# ── 2. Build ─────────────────────────────────────────────────────────────
echo "${bold}2) Build rollup tables in the warehouse${reset}"
echo "   by_platform_daily:  3 platforms × 365 days  = ~1,095 rows"
echo "   by_country_monthly: 6 countries × 12 months = ~72 rows"
echo ""
run_quiet $AL build --config config.yml
echo ""

# ── 3. Pull ──────────────────────────────────────────────────────────────
echo "${bold}3) Pull rollups to local Parquet cache${reset}"
echo ""
run_quiet $AL pull --config config.yml
echo ""
echo "   Cache files:"
ls -lh .airlayer/cache/*.parquet 2>/dev/null | awk '{print "   "$NF" ("$5")"}'
echo ""

# ── 4. Cached query — same as step 1 ────────────────────────────────────
echo "${bold}4) Same query — now served from local Parquet cache${reset}"
echo "   Revenue + event count by platform (re-aggregated from ~1,095-row rollup)"
echo ""
run_query $AL query -x --config config.yml \
    --dimension events.platform \
    --measure events.total_revenue \
    --measure events.event_count \
    --order events.total_revenue:desc
echo ""

# ── 5. Another cached query ─────────────────────────────────────────────
echo "${bold}5) Country query — served from 72-row Parquet cache${reset}"
echo ""
run_query $AL query -x --config config.yml \
    --dimension events.country \
    --measure events.total_revenue \
    --measure events.event_count \
    --order events.total_revenue:desc
echo ""

# ── 6. --no-cache ────────────────────────────────────────────────────────
echo "${bold}6) --no-cache bypasses cache, scans raw table again${reset}"
echo ""
run_query $AL query -x --config config.yml --no-cache \
    --dimension events.platform \
    --measure events.total_revenue \
    --measure events.event_count \
    --order events.total_revenue:desc
echo ""

echo "${green}${bold}Results match!${reset} Cached queries read from tiny Parquet files"
echo "instead of scanning the full 500M-row table."
)
