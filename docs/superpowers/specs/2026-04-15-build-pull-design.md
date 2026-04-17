# Pre-Aggregation: `airlayer build` & `airlayer pull`

## Overview

Adds pre-aggregation support to airlayer, inspired by Airbnb's Druid/Minerva pattern. The semantic layer builds cached rollup tables in the warehouse, optionally pulls them to local Parquet files, and transparently serves queries from the fastest available source.

## Schema: `pre_aggregations` in `.view.yml`

New optional field on views:

```yaml
name: orders
datasource: warehouse
table: orders
dimensions:
  - name: region
    type: string
    expr: region
  - name: status
    type: string
    expr: status
  - name: created_at
    type: datetime
    expr: created_at
measures:
  - name: total_revenue
    type: sum
    expr: revenue
  - name: unique_customers
    type: count_distinct
    expr: customer_id
  - name: median_order_value
    type: median
    expr: revenue

pre_aggregations:
  - name: by_region_monthly
    dimensions: [region]
    measures: [total_revenue, unique_customers]
    time_dimension: created_at
    granularity: month
  - name: by_status_daily
    dimensions: [region, status]
    measures: [total_revenue, median_order_value]
    time_dimension: created_at
    granularity: day
```

When `pre_aggregations` is **omitted**, `build` generates a default rollup: all dimensions x all measures x finest time granularity (day).

## `airlayer build` command

### CLI interface

```
airlayer build [OPTIONS]
  --schema <name>       Target schema name (default: AIRLAYER, or from config.yml)
  --database <name>     Which database to build against (default: first in config.yml)
  --view <name>         Build only a specific view (default: all views)
  --dry-run             Print the CTAS statements without executing
```

### What it does

1. Parses all views, resolves `pre_aggregations` (or generates defaults).
2. For each rollup, executes `CREATE TABLE <schema>.<view>__<hash>__<YYYYMMDD> AS SELECT ...`:
   - Dimensions in the rollup
   - Aggregated measures with type-specific column strategy (see below)
   - Time dimension truncated to the specified granularity
   - Appropriate GROUP BY clause
3. Upserts a row in `<schema>.__manifest` with metadata.
4. Does NOT drop old tables (stale cleanup is a separate concern).

### Warehouse table naming

```
<schema>.<view_name>__<rollup_hash>__<YYYYMMDD>
```

- `view_name`: the view it came from
- `rollup_hash`: deterministic short hash (first 8 chars of SHA-256) of the canonical representation: sorted dimension names + sorted measure names + time_dimension + granularity. The same rollup definition always produces the same hash.
- `YYYYMMDD`: build date

### Manifest table

```sql
CREATE TABLE IF NOT EXISTS <schema>.__manifest (
  view_name VARCHAR,
  rollup_name VARCHAR,
  rollup_hash VARCHAR,
  table_name VARCHAR,
  dimensions VARCHAR,       -- JSON array of dimension names
  measures VARCHAR,         -- JSON array of {name, type, columns}
  time_dimension VARCHAR,
  granularity VARCHAR,
  build_date DATE,
  PRIMARY KEY (view_name, rollup_name)
)
```

Upsert semantics: rebuilds replace the previous entry for the same `(view_name, rollup_name)`.

## Re-aggregation column strategy

The columns stored in pre-aggregated tables depend on measure type:

| Measure type | Columns stored | Grouping | Re-aggregation formula |
|---|---|---|---|
| `sum` | `<name>__sum` | dims + time | `SUM(<name>__sum)` |
| `count` | `<name>__count` | dims + time | `SUM(<name>__count)` |
| `avg` | `<name>__sum`, `<name>__count` | dims + time | `SUM(<name>__sum) / SUM(<name>__count)` |
| `min` | `<name>__min` | dims + time | `MIN(<name>__min)` |
| `max` | `<name>__max` | dims + time | `MAX(<name>__max)` |
| `count_distinct` | raw `<expr>` column | dims + time + expr | `COUNT(DISTINCT <expr>)` |
| `median` | raw `<expr>`, `<expr>__freq` | dims + time + expr | Expand by freq, then `MEDIAN` |
| `count_distinct_approx` | raw `<expr>` column | dims + time + expr | `COUNT(DISTINCT <expr>)` (exact locally) |
| `custom` | Not pre-aggregable | N/A | Query falls through to raw SQL |
| `number` | `<name>__value` | dims + time | Pass-through |

For `count_distinct` and `median`, the rollup groups by `dims + time + expr_column`, which deduplicates values rather than storing every raw row. For `median`, a `__freq` column (`COUNT(*)`) preserves frequency information for accurate re-aggregation.

A query that includes a `custom` measure cannot be served from cache and falls through to raw SQL automatically.

## `airlayer pull` command

### CLI interface

```
airlayer pull [OPTIONS]
  --schema <name>       Source schema name (default: AIRLAYER, or from config.yml)
  --database <name>     Which database to pull from (default: first in config.yml)
  --view <name>         Pull only a specific view (default: all)
```

### What it does

1. Connects to the warehouse, reads `<schema>.__manifest` to discover available rollups.
2. For each rollup (filtered by `--view` if specified):
   - Finds the latest entry per `(view_name, rollup_name)` by `build_date`.
   - Runs `SELECT * FROM <schema>.<table_name>`.
   - Writes result to `.airlayer/cache/<view_name>__<rollup_hash>.parquet` via DuckDB's Parquet writer.
3. Writes `.airlayer/cache/manifest.json` with full metadata from the warehouse manifest.

### Local cache structure

```
.airlayer/
  cache/
    manifest.json
    orders__a1b2c3.parquet
    orders__d4e5f6.parquet
    events__g7h8i9.parquet
```

### Local manifest format

```json
{
  "pulled_at": "2026-04-15T10:30:00Z",
  "source_database": "warehouse",
  "rollups": [
    {
      "view_name": "orders",
      "rollup_name": "by_region_monthly",
      "rollup_hash": "a1b2c3",
      "file": "orders__a1b2c3.parquet",
      "dimensions": ["region"],
      "measures": [
        {"name": "total_revenue", "type": "sum", "columns": ["total_revenue__sum"]},
        {"name": "unique_customers", "type": "count_distinct", "columns": ["customer_id"]}
      ],
      "time_dimension": "created_at",
      "granularity": "month",
      "build_date": "2026-04-15"
    }
  ]
}
```

### Cache lifecycle

- `pull` writes to `.airlayer/cache/`, overwriting previous data.
- Cache persists until the user deletes it or runs `pull` again.
- `airlayer init` adds `.airlayer/cache/` to `.gitignore`.

## Modified `airlayer query` — cache-aware execution

### Execution resolution order (when `--execute` is used)

1. **Local cache**: Check `.airlayer/cache/manifest.json`. If a rollup covers the query, execute via DuckDB + Parquet.
2. **Warehouse pre-agg**: Check `<schema>.__manifest` in the warehouse. If a rollup covers the query, rewrite SQL to target the pre-aggregated table.
3. **Raw SQL**: Compile and execute against source tables (today's behavior).

### Coverage check

A cached rollup covers a query if:
- All requested dimensions are in the rollup's dimensions.
- All requested measures are in the rollup's measures (and none are `custom` type).
- Time dimension matches.
- Granularity matches or is finer (coarser queries can re-aggregate from finer data for composable measures).

### Re-aggregation

When serving from cache at a coarser granularity than stored:
- Additive measures (`sum`, `count`, `min`, `max`): re-aggregate directly.
- `avg`: recompute as `SUM(sum_col) / SUM(count_col)`.
- `count_distinct`: `COUNT(DISTINCT expr_col)` on stored distinct values.
- `median`: expand by `__freq`, then `MEDIAN`.

### CLI flags

- `--no-cache`: skip both local and warehouse pre-agg layers, go straight to raw SQL.
- Default: tries all three layers in order.

### QueryEnvelope

The original compiled SQL (against raw tables) is still included in the QueryEnvelope for transparency, even when the query is served from cache. An additional field indicates the source layer used (`local_cache`, `warehouse_preagg`, or `raw`).

## Config integration

### `config.yml` additions

```yaml
databases:
  - name: warehouse
    type: clickhouse
    host: localhost
    # ... existing connection fields

pre_aggregations:
  schema: AIRLAYER          # default schema name
  database: warehouse       # optional, defaults to first database
```

### `.gitignore`

`airlayer init` adds `.airlayer/cache/` to `.gitignore`.

## Initial implementation scope

- **Warehouse**: ClickHouse only (testable via Docker)
- **Local cache**: DuckDB + Parquet (already a dependency)
- **Abstraction**: designed for extensibility to other warehouses

## End-to-end workflows

### CI/CD scheduled job
```bash
airlayer build    # pre-aggregate all views into AIRLAYER schema
```

### Developer local workflow
```bash
airlayer pull                     # download latest pre-agg Parquet files
airlayer query -q '...' -x       # uses local cache -> warehouse pre-agg -> raw
```

### Query-only (no pull)
```bash
airlayer query -q '...' -x       # no local cache -> checks warehouse pre-agg -> raw
```

### Bypass cache
```bash
airlayer query -q '...' -x --no-cache   # skip cache layers, go straight to raw SQL
```
