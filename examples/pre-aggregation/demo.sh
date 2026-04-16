#!/usr/bin/env bash
# Pre-aggregation demo: shows the speed difference between raw queries
# and queries served from a local Parquet cache.
#
# Usage:
#   ./seed.sh        # one-time: create 1M-row DuckDB warehouse
#   ./demo.sh        # run the demo
# Usage: ./demo.sh  (do NOT dot-source with ". ./demo.sh")
(
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT="$(cd ../.. && pwd)"

# ── resolve airlayer binary ──────────────────────────────────────────────
# Prefer repo build over system install (system may lack exec features).
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
echo "Using: $AL"
echo ""

# ── helpers ──────────────────────────────────────────────────────────────
bold=$(tput bold)
dim=$(tput setaf 8)
reset=$(tput sgr0)

timed() {
    local start end elapsed
    start=$(python3 -c 'import time; print(time.time())')
    "$@" > /tmp/airlayer_demo_out.json 2>/dev/null
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(f'{($end - $start)*1000:.0f}')")
    local rows
    rows=$(python3 -c "import json; d=json.load(open('/tmp/airlayer_demo_out.json')); print(d.get('row_count',''))" 2>/dev/null || true)
    if [ -n "$rows" ]; then
        echo "${dim}  ⏱  ${elapsed} ms  (${rows} rows)${reset}"
    else
        echo "${dim}  ⏱  ${elapsed} ms${reset}"
    fi
    echo ""
}

# ── JSON queries (time_dimensions require -q) ────────────────────────────
Q_PLATFORM_MONTHLY='{
  "measures": ["events.event_count", "events.total_revenue"],
  "dimensions": ["events.platform"],
  "time_dimensions": [{"dimension": "events.created_at", "granularity": "month"}],
  "order": [{"id": "events.total_revenue", "direction": "desc"}]
}'

Q_COUNTRY_MONTHLY='{
  "measures": ["events.event_count", "events.total_revenue"],
  "dimensions": ["events.country"],
  "time_dimensions": [{"dimension": "events.created_at", "granularity": "month"}],
  "order": [{"id": "events.total_revenue", "direction": "desc"}]
}'

# ── check prerequisites ─────────────────────────────────────────────────
if [ ! -f data/warehouse.duckdb ]; then
    echo "Run ./seed.sh first to create the 1M-row warehouse."
    exit 1
fi

# Clean any prior cache
rm -rf .airlayer

echo "${bold}╔══════════════════════════════════════════════════════════════╗${reset}"
echo "${bold}║        Pre-aggregation Demo  (1M rows in DuckDB)           ║${reset}"
echo "${bold}╚══════════════════════════════════════════════════════════════╝${reset}"
echo ""

# ── 1. Raw queries (no cache) ────────────────────────────────────────────
echo "${bold}1) Raw query — scans 1M rows${reset}"
echo "   Revenue by platform, monthly"
echo ""
timed $AL query -x --config config.yml --no-cache -q "$Q_PLATFORM_MONTHLY"

echo "${bold}2) Raw query — scans 1M rows${reset}"
echo "   Revenue by country, monthly"
echo ""
timed $AL query -x --config config.yml --no-cache -q "$Q_COUNTRY_MONTHLY"

# ── 2. Build pre-aggregated tables ──────────────────────────────────────
echo "${bold}3) Build rollup tables in the warehouse${reset}"
echo "   by_platform_daily: 3 platforms × 365 days = ~1,095 rows"
echo "   by_country_monthly: 6 countries × 12 months = ~72 rows"
echo ""
timed $AL build --config config.yml

# ── 3. Pull to local Parquet cache ──────────────────────────────────────
echo "${bold}4) Pull rollups to local Parquet cache${reset}"
echo ""
timed $AL pull --config config.yml

echo "   Cache contents:"
ls -lh .airlayer/cache/*.parquet 2>/dev/null || echo "   (no files)"
echo ""

# ── 4. Same queries — now from cache ────────────────────────────────────
echo "${bold}5) Cached query — reads ~1,095-row Parquet instead of 1M rows${reset}"
echo "   Revenue by platform, monthly (same as step 1)"
echo ""
timed $AL query -x --config config.yml -q "$Q_PLATFORM_MONTHLY"

echo "${bold}6) Cached query — reads ~72-row Parquet instead of 1M rows${reset}"
echo "   Revenue by country, monthly (same as step 2)"
echo ""
timed $AL query -x --config config.yml -q "$Q_COUNTRY_MONTHLY"

# ── 5. --no-cache bypasses everything ────────────────────────────────────
echo "${bold}7) --no-cache bypasses the cache, scans raw table again${reset}"
echo ""
timed $AL query -x --config config.yml --no-cache \
    --dimension events.platform \
    --measure events.total_revenue \
    --order events.total_revenue:desc

echo "${bold}Done!${reset}"
echo "Cached queries read from tiny Parquet files (~1K rows)"
echo "instead of scanning the full 1M-row table."
)
