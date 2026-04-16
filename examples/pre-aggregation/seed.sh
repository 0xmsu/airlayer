#!/usr/bin/env bash
# Seed a DuckDB warehouse with 500M events for the pre-aggregation demo.
# NOTE: The generated database requires ~8-10 GB of disk space.
#
# Usage: ./seed.sh  (do NOT dot-source with ". ./seed.sh")
(
set -euo pipefail
cd "$(dirname "$0")"

mkdir -p data

echo "Seeding 500,000,000 events into data/warehouse.duckdb ..."

duckdb data/warehouse.duckdb <<'SQL'
DROP TABLE IF EXISTS events;

CREATE TABLE events AS
SELECT
    'e' || LPAD(CAST(i AS VARCHAR), 9, '0') AS event_id,
    CASE i % 5
        WHEN 0 THEN 'page_view'
        WHEN 1 THEN 'click'
        WHEN 2 THEN 'purchase'
        WHEN 3 THEN 'signup'
        WHEN 4 THEN 'share'
    END AS event_type,
    'u' || LPAD(CAST((i % 500000) AS VARCHAR), 6, '0') AS user_id,
    CAST('2024-01-01' AS TIMESTAMP) + INTERVAL (i % 365) DAY
        + INTERVAL (i % 24) HOUR AS created_at,
    CASE i % 6
        WHEN 0 THEN 'US'
        WHEN 1 THEN 'UK'
        WHEN 2 THEN 'DE'
        WHEN 3 THEN 'JP'
        WHEN 4 THEN 'BR'
        WHEN 5 THEN 'AU'
    END AS country,
    CASE i % 3
        WHEN 0 THEN 'web'
        WHEN 1 THEN 'ios'
        WHEN 2 THEN 'android'
    END AS platform,
    CASE WHEN i % 5 = 2 THEN (i % 500) * 100 ELSE 0 END AS revenue_cents
FROM generate_series(1, 500000000) t(i);

SELECT COUNT(*) || ' rows seeded' FROM events;
SQL

echo "Done. Database: data/warehouse.duckdb"
)
