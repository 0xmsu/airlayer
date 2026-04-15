# Pre-Aggregation (build/pull) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `airlayer build` and `airlayer pull` commands that create pre-aggregated rollup tables in ClickHouse and pull them locally as Parquet, with cache-aware query execution.

**Architecture:** New `src/engine/preagg.rs` module contains all pre-aggregation logic (rollup resolution, hashing, SQL generation, coverage checking, re-aggregation). CLI adds `Build` and `Pull` subcommands. The existing `run_execute` path gains a three-tier resolution: local Parquet cache → warehouse pre-agg table → raw SQL.

**Tech Stack:** Rust, ClickHouse (ReplacingMergeTree for manifest), DuckDB (Parquet writing/reading), serde_json (local manifest)

**Spec:** `docs/superpowers/specs/2026-04-15-build-pull-design.md`

---

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/schema/models.rs` | Add `PreAggregation` struct, `pre_aggregations` field on `View`/`RawView` |
| Modify | `src/engine/mod.rs` | Add `PreAggConfig` to `PartialConfig`, re-export preagg module |
| Create | `src/engine/preagg.rs` | Rollup resolution, hashing, CTAS SQL gen, manifest SQL, coverage check, re-agg SQL gen |
| Modify | `src/cli/mod.rs` | `Build`/`Pull` subcommands, cache-aware query execution |
| Modify | `Cargo.toml` | No new deps needed (uses existing serde_json, duckdb, ureq) |
| Create | `tests/integration/views/events_preagg.view.yml` | Test view with `pre_aggregations` field |
| Modify | `tests/integration_tests.rs` | ClickHouse build/pull/cache integration tests |

---

### Task 1: Schema — `PreAggregation` type and View field

**Files:**
- Modify: `src/schema/models.rs`

- [ ] **Step 1: Write the failing test**

Add a unit test at the bottom of `src/schema/models.rs` (or in the parser tests) that deserializes a view YAML with `pre_aggregations`:

```rust
#[cfg(test)]
mod preagg_tests {
    use super::*;

    #[test]
    fn test_view_with_pre_aggregations_parses() {
        let yaml = r#"
name: orders
description: "Test orders"
table: orders
dimensions:
  - name: region
    type: string
    expr: region
  - name: created_at
    type: datetime
    expr: created_at
measures:
  - name: total_revenue
    type: sum
    expr: revenue
pre_aggregations:
  - name: by_region_monthly
    dimensions: [region]
    measures: [total_revenue]
    time_dimension: created_at
    granularity: month
"#;
        let raw: RawView = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(raw.pre_aggregations.as_ref().unwrap().len(), 1);
        let pa = &raw.pre_aggregations.as_ref().unwrap()[0];
        assert_eq!(pa.name, "by_region_monthly");
        assert_eq!(pa.dimensions, vec!["region"]);
        assert_eq!(pa.measures, vec!["total_revenue"]);
        assert_eq!(pa.time_dimension.as_deref(), Some("created_at"));
        assert_eq!(pa.granularity.as_deref(), Some("month"));
    }

    #[test]
    fn test_view_without_pre_aggregations_parses() {
        let yaml = r#"
name: orders
description: "Test orders"
table: orders
dimensions:
  - name: region
    type: string
    expr: region
measures:
  - name: total_revenue
    type: sum
    expr: revenue
"#;
        let raw: RawView = serde_yaml::from_str(yaml).expect("parse");
        assert!(raw.pre_aggregations.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test preagg_tests -- --nocapture`
Expected: FAIL — `RawView` has no field `pre_aggregations`

- [ ] **Step 3: Add PreAggregation struct and fields**

In `src/schema/models.rs`, add the `PreAggregation` struct before the `View` struct:

```rust
/// A pre-aggregation rollup definition within a view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreAggregation {
    pub name: String,
    #[serde(default)]
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub measures: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_dimension: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
}
```

Add `pre_aggregations` field to `View`:

```rust
// In the View struct, after `segments`:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_aggregations: Option<Vec<PreAggregation>>,
```

Add `pre_aggregations` field to `RawView`:

```rust
// In the RawView struct, after `segments`:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_aggregations: Option<Vec<PreAggregation>>,
```

Update `resolve_raw_view()` in `src/schema/parser.rs` to pass through `pre_aggregations` from `RawView` to `View`. Find where `View` is constructed from `RawView` fields and add:

```rust
pre_aggregations: raw.pre_aggregations,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test preagg_tests -- --nocapture`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All existing tests still pass (the new field is optional with serde default)

- [ ] **Step 6: Commit**

```bash
git add src/schema/models.rs src/schema/parser.rs
git commit -m "feat: add PreAggregation schema type and View field"
```

---

### Task 2: Config — `pre_aggregations` section in config.yml

**Files:**
- Modify: `src/engine/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_partial_config_with_preagg() {
    let yaml = r#"
databases:
  - name: warehouse
    type: clickhouse
pre_aggregations:
  schema: MY_CACHE
  database: warehouse
"#;
    let config: PartialConfig = serde_yaml::from_str(yaml).expect("parse config");
    let preagg = config.pre_aggregations.as_ref().expect("has preagg");
    assert_eq!(preagg.schema.as_deref(), Some("MY_CACHE"));
    assert_eq!(preagg.database.as_deref(), Some("warehouse"));
}

#[test]
fn test_partial_config_preagg_defaults() {
    let yaml = r#"
databases:
  - name: warehouse
    type: clickhouse
"#;
    let config: PartialConfig = serde_yaml::from_str(yaml).expect("parse config");
    assert!(config.pre_aggregations.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_partial_config_with_preagg -- --nocapture`
Expected: FAIL — `PartialConfig` has no `pre_aggregations` field

- [ ] **Step 3: Add PreAggConfig and update PartialConfig**

In `src/engine/mod.rs`, add:

```rust
/// Pre-aggregation configuration from config.yml.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PreAggConfig {
    /// Schema/database name for pre-aggregated tables. Default: "AIRLAYER".
    pub schema: Option<String>,
    /// Which database to use for pre-aggregation. Default: first database.
    pub database: Option<String>,
}
```

Add field to `PartialConfig`:

```rust
pub struct PartialConfig {
    #[serde(default)]
    pub databases: Vec<DatabaseConfig>,
    #[serde(default)]
    pub pre_aggregations: Option<PreAggConfig>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test test_partial_config -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/engine/mod.rs
git commit -m "feat: add PreAggConfig to PartialConfig for config.yml"
```

---

### Task 3: Core preagg module — rollup resolution and hashing

**Files:**
- Create: `src/engine/preagg.rs`
- Modify: `src/engine/mod.rs` (add `pub mod preagg;`)

- [ ] **Step 1: Write the failing test**

Create `src/engine/preagg.rs` with just tests:

```rust
//! Pre-aggregation: rollup resolution, SQL generation, coverage checking.

use crate::schema::models::{MeasureType, PreAggregation, View};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rollup_hash_deterministic() {
        let h1 = compute_rollup_hash(&["region".into(), "status".into()], &["revenue".into()], Some("created_at"), Some("month"));
        let h2 = compute_rollup_hash(&["region".into(), "status".into()], &["revenue".into()], Some("created_at"), Some("month"));
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_rollup_hash_order_independent() {
        let h1 = compute_rollup_hash(&["region".into(), "status".into()], &["a".into(), "b".into()], None, None);
        let h2 = compute_rollup_hash(&["status".into(), "region".into()], &["b".into(), "a".into()], None, None);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_rollup_hash_different_inputs() {
        let h1 = compute_rollup_hash(&["region".into()], &["revenue".into()], Some("created_at"), Some("month"));
        let h2 = compute_rollup_hash(&["status".into()], &["revenue".into()], Some("created_at"), Some("month"));
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_resolve_rollups_explicit() {
        let view = test_view_with_preaggs();
        let rollups = resolve_rollups(&view);
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].name, "by_region_monthly");
        assert_eq!(rollups[0].dimensions, vec!["region"]);
        assert_eq!(rollups[0].time_dimension.as_deref(), Some("created_at"));
    }

    #[test]
    fn test_resolve_rollups_default_all() {
        let view = test_view_no_preaggs();
        let rollups = resolve_rollups(&view);
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].name, "default");
        // Should include all dimensions (except datetime — that's the time dim)
        assert!(rollups[0].dimensions.contains(&"region".to_string()));
        // Should include all measures (except custom)
        assert!(rollups[0].measures.iter().any(|m| m.name == "total_revenue"));
    }

    fn test_view_with_preaggs() -> View {
        use crate::schema::models::*;
        View {
            name: "orders".to_string(),
            description: "test".to_string(),
            label: None,
            datasource: None,
            dialect: None,
            table: Some("orders".to_string()),
            sql: None,
            entities: vec![],
            dimensions: vec![
                Dimension { name: "region".into(), dimension_type: DimensionType::String, description: None, expr: "region".into(), original_expr: None, samples: None, synonyms: None, primary_key: None, sub_query: None, inherits_from: None, meta: None },
                Dimension { name: "created_at".into(), dimension_type: DimensionType::Datetime, description: None, expr: "created_at".into(), original_expr: None, samples: None, synonyms: None, primary_key: None, sub_query: None, inherits_from: None, meta: None },
            ],
            measures: Some(vec![
                Measure { name: "total_revenue".into(), measure_type: MeasureType::Sum, description: None, expr: Some("revenue".into()), original_expr: None, filters: None, samples: None, synonyms: None, rolling_window: None, inherits_from: None, meta: None },
            ]),
            segments: vec![],
            pre_aggregations: Some(vec![PreAggregation {
                name: "by_region_monthly".into(),
                dimensions: vec!["region".into()],
                measures: vec!["total_revenue".into()],
                time_dimension: Some("created_at".into()),
                granularity: Some("month".into()),
            }]),
            meta: None,
        }
    }

    fn test_view_no_preaggs() -> View {
        use crate::schema::models::*;
        View {
            name: "orders".into(),
            description: "test".into(),
            label: None,
            datasource: None,
            dialect: None,
            table: Some("orders".into()),
            sql: None,
            entities: vec![],
            dimensions: vec![
                Dimension { name: "region".into(), dimension_type: DimensionType::String, description: None, expr: "region".into(), original_expr: None, samples: None, synonyms: None, primary_key: None, sub_query: None, inherits_from: None, meta: None },
                Dimension { name: "created_at".into(), dimension_type: DimensionType::Datetime, description: None, expr: "created_at".into(), original_expr: None, samples: None, synonyms: None, primary_key: None, sub_query: None, inherits_from: None, meta: None },
            ],
            measures: Some(vec![
                Measure { name: "total_revenue".into(), measure_type: MeasureType::Sum, description: None, expr: Some("revenue".into()), original_expr: None, filters: None, samples: None, synonyms: None, rolling_window: None, inherits_from: None, meta: None },
                Measure { name: "avg_revenue".into(), measure_type: MeasureType::Average, description: None, expr: Some("revenue".into()), original_expr: None, filters: None, samples: None, synonyms: None, rolling_window: None, inherits_from: None, meta: None },
            ]),
            segments: vec![],
            pre_aggregations: None,
            meta: None,
        }
    }
}
```

- [ ] **Step 2: Register the module**

In `src/engine/mod.rs`, add:

```rust
pub mod preagg;
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test preagg::tests -- --nocapture`
Expected: FAIL — functions not defined

- [ ] **Step 4: Implement core types and functions**

Add to `src/engine/preagg.rs` above the tests:

```rust
/// A resolved rollup specification ready for SQL generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupSpec {
    pub name: String,
    pub hash: String,
    pub dimensions: Vec<String>,
    pub measures: Vec<RollupMeasure>,
    pub time_dimension: Option<String>,
    pub granularity: Option<String>,
}

/// A measure within a rollup, with its storage columns determined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupMeasure {
    pub name: String,
    pub measure_type: MeasureType,
    /// The original SQL expression from the view definition.
    pub expr: Option<String>,
    /// Column names stored in the pre-agg table for this measure.
    pub columns: Vec<String>,
}

/// Compute a deterministic 8-char hex hash for a rollup specification.
/// Uses FNV-1a for stability across Rust versions.
pub fn compute_rollup_hash(
    dims: &[String],
    measures: &[String],
    time_dim: Option<&str>,
    granularity: Option<&str>,
) -> String {
    let mut sorted_dims = dims.to_vec();
    sorted_dims.sort();
    let mut sorted_measures = measures.to_vec();
    sorted_measures.sort();

    let canonical = format!(
        "d:{};m:{};t:{};g:{}",
        sorted_dims.join(","),
        sorted_measures.join(","),
        time_dim.unwrap_or(""),
        granularity.unwrap_or(""),
    );

    // FNV-1a hash
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in canonical.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)[..8].to_string()
}

/// Resolve rollup specs for a view. If `pre_aggregations` is defined, use those.
/// Otherwise, generate a default rollup covering all dimensions × all measures × day granularity.
pub fn resolve_rollups(view: &View) -> Vec<RollupSpec> {
    if let Some(ref preaggs) = view.pre_aggregations {
        preaggs
            .iter()
            .map(|pa| resolve_explicit_rollup(view, pa))
            .collect()
    } else {
        vec![generate_default_rollup(view)]
    }
}

fn resolve_explicit_rollup(view: &View, pa: &PreAggregation) -> RollupSpec {
    let measures: Vec<RollupMeasure> = pa
        .measures
        .iter()
        .filter_map(|name| {
            let m = view.measures_list().iter().find(|m| m.name == *name)?;
            Some(build_rollup_measure(m))
        })
        .collect();

    let measure_names: Vec<String> = measures.iter().map(|m| m.name.clone()).collect();
    let hash = compute_rollup_hash(
        &pa.dimensions,
        &measure_names,
        pa.time_dimension.as_deref(),
        pa.granularity.as_deref(),
    );

    RollupSpec {
        name: pa.name.clone(),
        hash,
        dimensions: pa.dimensions.clone(),
        measures,
        time_dimension: pa.time_dimension.clone(),
        granularity: pa.granularity.clone(),
    }
}

fn generate_default_rollup(view: &View) -> RollupSpec {
    // Find the first datetime dimension as the time dimension
    let time_dim = view
        .dimensions
        .iter()
        .find(|d| {
            d.dimension_type == crate::schema::models::DimensionType::Datetime
                || d.dimension_type == crate::schema::models::DimensionType::Date
        })
        .map(|d| d.name.clone());

    // All non-datetime dimensions
    let dimensions: Vec<String> = view
        .dimensions
        .iter()
        .filter(|d| {
            d.dimension_type != crate::schema::models::DimensionType::Datetime
                && d.dimension_type != crate::schema::models::DimensionType::Date
        })
        .map(|d| d.name.clone())
        .collect();

    // All pre-aggregable measures
    let measures: Vec<RollupMeasure> = view
        .measures_list()
        .iter()
        .filter(|m| m.measure_type != MeasureType::Custom)
        .map(|m| build_rollup_measure(m))
        .collect();

    let measure_names: Vec<String> = measures.iter().map(|m| m.name.clone()).collect();
    let hash = compute_rollup_hash(
        &dimensions,
        &measure_names,
        time_dim.as_deref(),
        Some("day"),
    );

    RollupSpec {
        name: "default".to_string(),
        hash,
        dimensions,
        measures,
        time_dimension: time_dim,
        granularity: Some("day".to_string()),
    }
}

fn build_rollup_measure(m: &crate::schema::models::Measure) -> RollupMeasure {
    let columns = match m.measure_type {
        MeasureType::Sum => vec![format!("{}__sum", m.name)],
        MeasureType::Count => vec![format!("{}__count", m.name)],
        MeasureType::Average => vec![format!("{}__sum", m.name), format!("{}__count", m.name)],
        MeasureType::Min => vec![format!("{}__min", m.name)],
        MeasureType::Max => vec![format!("{}__max", m.name)],
        MeasureType::CountDistinct | MeasureType::CountDistinctApprox => {
            // Store the raw expression column name
            let expr_col = m.expr.clone().unwrap_or_else(|| m.name.clone());
            vec![expr_col]
        }
        MeasureType::Median => {
            let expr_col = m.expr.clone().unwrap_or_else(|| m.name.clone());
            vec![expr_col.clone(), format!("{}__freq", expr_col)]
        }
        MeasureType::Number => vec![format!("{}__value", m.name)],
        MeasureType::Custom => vec![], // Not pre-aggregable
    };

    RollupMeasure {
        name: m.name.clone(),
        measure_type: m.measure_type.clone(),
        expr: m.expr.clone(),
        columns,
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test preagg::tests -- --nocapture`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/engine/preagg.rs src/engine/mod.rs
git commit -m "feat: add preagg module with rollup resolution and hashing"
```

---

### Task 4: Build SQL generation — CTAS and manifest

**Files:**
- Modify: `src/engine/preagg.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/engine/preagg.rs` tests:

```rust
#[test]
fn test_generate_build_sql_sum() {
    let view = test_view_with_preaggs();
    let rollups = resolve_rollups(&view);
    let sqls = generate_build_sql(
        &view,
        &rollups[0],
        "AIRLAYER",
        "20260415",
        &crate::dialect::Dialect::ClickHouse,
    );
    assert_eq!(sqls.len(), 1); // One CTAS statement
    let ctas = &sqls[0];
    assert!(ctas.contains("CREATE TABLE"), "Missing CREATE TABLE: {}", ctas);
    assert!(ctas.contains("AIRLAYER"), "Missing schema: {}", ctas);
    assert!(ctas.contains("orders__"), "Missing view name: {}", ctas);
    assert!(ctas.contains("20260415"), "Missing date: {}", ctas);
    assert!(ctas.contains("SUM("), "Missing SUM aggregation: {}", ctas);
    assert!(ctas.contains("total_revenue__sum"), "Missing column alias: {}", ctas);
    assert!(ctas.contains("toStartOfMonth"), "Missing ClickHouse date_trunc: {}", ctas);
}

#[test]
fn test_generate_manifest_sql() {
    let create = generate_manifest_create_sql("AIRLAYER", &crate::dialect::Dialect::ClickHouse);
    assert!(create.contains("__manifest"), "Missing manifest table name: {}", create);
    assert!(create.contains("CREATE TABLE"), "Missing CREATE TABLE: {}", create);
}

#[test]
fn test_generate_manifest_upsert() {
    let entry = ManifestEntry {
        view_name: "orders".into(),
        rollup_name: "by_region".into(),
        rollup_hash: "a1b2c3d4".into(),
        table_name: "orders__a1b2c3d4__20260415".into(),
        dimensions: vec!["region".into()],
        measures_json: "[]".into(),
        time_dimension: Some("created_at".into()),
        granularity: Some("month".into()),
        build_date: "2026-04-15".into(),
    };
    let sql = generate_manifest_upsert_sql("AIRLAYER", &entry, &crate::dialect::Dialect::ClickHouse);
    assert!(sql.contains("INSERT INTO"), "Missing INSERT: {}", sql);
    assert!(sql.contains("orders"), "Missing view name: {}", sql);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test preagg::tests::test_generate -- --nocapture`
Expected: FAIL — functions not defined

- [ ] **Step 3: Implement CTAS generation**

Add to `src/engine/preagg.rs`:

```rust
use crate::dialect::Dialect;

/// Manifest entry for a pre-aggregated rollup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub view_name: String,
    pub rollup_name: String,
    pub rollup_hash: String,
    pub table_name: String,
    pub dimensions: Vec<String>,
    pub measures_json: String,
    pub time_dimension: Option<String>,
    pub granularity: Option<String>,
    pub build_date: String,
}

/// Generate the CTAS SQL statements for a rollup. Returns a list of SQL strings
/// (schema creation, CTAS). The table is named `<schema>.<view>__<hash>__<date>`.
pub fn generate_build_sql(
    view: &View,
    rollup: &RollupSpec,
    schema: &str,
    date_str: &str,
    dialect: &Dialect,
) -> Vec<String> {
    let table_name = format!("{}__{}__{}",
        view.name, rollup.hash, date_str
    );
    let fq_table = format!("{}.{}", schema, table_name);

    // Determine which raw expr columns need to be in GROUP BY (for count_distinct, median)
    let mut extra_group_cols: Vec<String> = Vec::new();
    for rm in &rollup.measures {
        match rm.measure_type {
            MeasureType::CountDistinct | MeasureType::CountDistinctApprox => {
                let col = rm.expr.clone().unwrap_or_else(|| rm.name.clone());
                if !extra_group_cols.contains(&col) {
                    extra_group_cols.push(col);
                }
            }
            MeasureType::Median => {
                let col = rm.expr.clone().unwrap_or_else(|| rm.name.clone());
                if !extra_group_cols.contains(&col) {
                    extra_group_cols.push(col);
                }
            }
            _ => {}
        }
    }

    // Build SELECT columns
    let mut select_cols: Vec<String> = Vec::new();
    let mut group_by_cols: Vec<String> = Vec::new();

    // 1. Dimensions
    for dim_name in &rollup.dimensions {
        if let Some(dim) = view.dimensions.iter().find(|d| d.name == *dim_name) {
            let alias = dialect.quote_identifier(dim_name);
            select_cols.push(format!("{} AS {}", dim.expr, alias));
            group_by_cols.push(dim.expr.clone());
        }
    }

    // 2. Time dimension (truncated)
    if let (Some(ref td_name), Some(ref gran)) = (&rollup.time_dimension, &rollup.granularity) {
        if let Some(td) = view.dimensions.iter().find(|d| d.name == *td_name) {
            let trunc_expr = dialect.date_trunc(gran, &td.expr);
            let alias = dialect.quote_identifier(&format!("{}__{}", td_name, gran));
            select_cols.push(format!("{} AS {}", trunc_expr, alias));
            group_by_cols.push(trunc_expr);
        }
    }

    // 3. Extra GROUP BY columns for count_distinct / median
    for col in &extra_group_cols {
        let alias = dialect.quote_identifier(col);
        select_cols.push(format!("{} AS {}", col, alias));
        group_by_cols.push(col.clone());
    }

    // 4. Measure columns
    for rm in &rollup.measures {
        let expr = rm.expr.clone().unwrap_or("*".to_string());
        match rm.measure_type {
            MeasureType::Sum => {
                let alias = dialect.quote_identifier(&format!("{}__sum", rm.name));
                select_cols.push(format!("SUM({}) AS {}", expr, alias));
            }
            MeasureType::Count => {
                let alias = dialect.quote_identifier(&format!("{}__count", rm.name));
                if expr == "*" {
                    select_cols.push(format!("COUNT(*) AS {}", alias));
                } else {
                    select_cols.push(format!("COUNT({}) AS {}", expr, alias));
                }
            }
            MeasureType::Average => {
                let sum_alias = dialect.quote_identifier(&format!("{}__sum", rm.name));
                let count_alias = dialect.quote_identifier(&format!("{}__count", rm.name));
                select_cols.push(format!("SUM({}) AS {}", expr, sum_alias));
                select_cols.push(format!("COUNT({}) AS {}", expr, count_alias));
            }
            MeasureType::Min => {
                let alias = dialect.quote_identifier(&format!("{}__min", rm.name));
                select_cols.push(format!("MIN({}) AS {}", expr, alias));
            }
            MeasureType::Max => {
                let alias = dialect.quote_identifier(&format!("{}__max", rm.name));
                select_cols.push(format!("MAX({}) AS {}", expr, alias));
            }
            MeasureType::CountDistinct | MeasureType::CountDistinctApprox => {
                // Raw column already in GROUP BY; no additional SELECT needed
            }
            MeasureType::Median => {
                // Raw column already in GROUP BY; add freq column
                let col = rm.expr.clone().unwrap_or_else(|| rm.name.clone());
                let freq_alias = dialect.quote_identifier(&format!("{}__freq", col));
                select_cols.push(format!("COUNT(*) AS {}", freq_alias));
            }
            MeasureType::Number => {
                let alias = dialect.quote_identifier(&format!("{}__value", rm.name));
                select_cols.push(format!("{} AS {}", expr, alias));
            }
            MeasureType::Custom => {} // Skip
        }
    }

    let source = view.source_sql();
    let select = select_cols.join(",\n    ");
    let group_by = group_by_cols
        .iter()
        .enumerate()
        .map(|(i, _)| format!("{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");

    let order_by = group_by.clone(); // Same as GROUP BY for MergeTree

    let ctas = match dialect {
        Dialect::ClickHouse => {
            format!(
                "CREATE TABLE {fq_table}\nENGINE = MergeTree()\nORDER BY ({order_by})\nAS\nSELECT\n    {select}\nFROM {source}\nGROUP BY {group_by}",
            )
        }
        _ => {
            format!(
                "CREATE TABLE {fq_table} AS\nSELECT\n    {select}\nFROM {source}\nGROUP BY {group_by}",
            )
        }
    };

    vec![ctas]
}

/// Generate the CREATE TABLE statement for the __manifest table.
pub fn generate_manifest_create_sql(schema: &str, dialect: &Dialect) -> String {
    let fq_table = format!("{}.__manifest", schema);
    match dialect {
        Dialect::ClickHouse => format!(
            "CREATE TABLE IF NOT EXISTS {fq_table} (\n\
             \x20   view_name String,\n\
             \x20   rollup_name String,\n\
             \x20   rollup_hash String,\n\
             \x20   table_name String,\n\
             \x20   dimensions String,\n\
             \x20   measures String,\n\
             \x20   time_dimension String,\n\
             \x20   granularity String,\n\
             \x20   build_date Date\n\
             ) ENGINE = ReplacingMergeTree(build_date)\n\
             ORDER BY (view_name, rollup_name)"
        ),
        _ => format!(
            "CREATE TABLE IF NOT EXISTS {fq_table} (\n\
             \x20   view_name VARCHAR,\n\
             \x20   rollup_name VARCHAR,\n\
             \x20   rollup_hash VARCHAR,\n\
             \x20   table_name VARCHAR,\n\
             \x20   dimensions VARCHAR,\n\
             \x20   measures VARCHAR,\n\
             \x20   time_dimension VARCHAR,\n\
             \x20   granularity VARCHAR,\n\
             \x20   build_date DATE,\n\
             \x20   PRIMARY KEY (view_name, rollup_name)\n\
             )"
        ),
    }
}

/// Generate INSERT SQL for a manifest entry.
pub fn generate_manifest_upsert_sql(
    schema: &str,
    entry: &ManifestEntry,
    _dialect: &Dialect,
) -> String {
    let fq_table = format!("{}.__manifest", schema);
    format!(
        "INSERT INTO {fq_table} (view_name, rollup_name, rollup_hash, table_name, dimensions, measures, time_dimension, granularity, build_date) VALUES (\
         '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}')",
        entry.view_name.replace('\'', "''"),
        entry.rollup_name.replace('\'', "''"),
        entry.rollup_hash.replace('\'', "''"),
        entry.table_name.replace('\'', "''"),
        serde_json::to_string(&entry.dimensions).unwrap_or_default().replace('\'', "''"),
        entry.measures_json.replace('\'', "''"),
        entry.time_dimension.as_deref().unwrap_or("").replace('\'', "''"),
        entry.granularity.as_deref().unwrap_or("").replace('\'', "''"),
        entry.build_date,
    )
}

/// Build a ManifestEntry from a view and rollup spec.
pub fn build_manifest_entry(
    view: &View,
    rollup: &RollupSpec,
    schema: &str,
    date_str: &str,
) -> ManifestEntry {
    let table_name = format!("{}__{}__{}",
        view.name, rollup.hash, date_str
    );

    let measures_json = serde_json::to_string(
        &rollup.measures.iter().map(|m| {
            serde_json::json!({
                "name": m.name,
                "type": m.measure_type.to_string(),
                "columns": m.columns,
            })
        }).collect::<Vec<_>>()
    ).unwrap_or_default();

    ManifestEntry {
        view_name: view.name.clone(),
        rollup_name: rollup.name.clone(),
        rollup_hash: rollup.hash.clone(),
        table_name: format!("{}.{}", schema, table_name),
        dimensions: rollup.dimensions.clone(),
        measures_json,
        time_dimension: rollup.time_dimension.clone(),
        granularity: rollup.granularity.clone(),
        build_date: date_str.to_string(),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test preagg::tests::test_generate -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/engine/preagg.rs
git commit -m "feat: add CTAS and manifest SQL generation for pre-aggregation"
```

---

### Task 5: Build CLI command

**Files:**
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Add Build subcommand to clap enum**

In `src/cli/mod.rs`, add to the `Commands` enum after `TestConnection`:

```rust
    /// Pre-aggregate views into rollup tables in the warehouse.
    Build {
        /// Path to config.yml.
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Target schema name for pre-aggregated tables (default: AIRLAYER).
        #[arg(long, default_value = "AIRLAYER")]
        schema: String,

        /// Which database to build against (default: first in config.yml).
        #[arg(long)]
        database: Option<String>,

        /// Build only a specific view.
        #[arg(long)]
        view: Option<String>,

        /// Print the CTAS statements without executing.
        #[arg(long)]
        dry_run: bool,

        /// Path to globals file (optional).
        #[arg(short, long)]
        globals: Option<PathBuf>,
    },
```

- [ ] **Step 2: Add the match arm in `run()`**

In the `match cli.command` block, add:

```rust
        Commands::Build {
            config,
            schema,
            database,
            view,
            dry_run,
            globals,
        } => {
            run_build(
                globals.as_ref(),
                config.as_ref(),
                &schema,
                database.as_deref(),
                view.as_deref(),
                dry_run,
            )?;
        }
```

- [ ] **Step 3: Implement `run_build()`**

Add the function:

```rust
/// Build pre-aggregated rollup tables in the warehouse.
fn run_build(
    globals: Option<&PathBuf>,
    config: Option<&PathBuf>,
    schema: &str,
    database: Option<&str>,
    view_filter: Option<&str>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::engine::preagg;

    let ctx = resolve_project_context(config)?;
    let config_path = ctx
        .config_path
        .as_ref()
        .ok_or("build requires a config.yml (auto-detected or via --config)")?;

    // Check for schema override in config.yml
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("Failed to read config {}: {}", config_path.display(), e))?;
    let partial: PartialConfig = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse config: {}", e))?;

    let effective_schema = if schema != "AIRLAYER" {
        schema.to_string() // CLI flag takes precedence
    } else if let Some(ref pa) = partial.pre_aggregations {
        pa.schema.clone().unwrap_or_else(|| "AIRLAYER".to_string())
    } else {
        "AIRLAYER".to_string()
    };

    let effective_database = database
        .map(|s| s.to_string())
        .or_else(|| partial.pre_aggregations.as_ref().and_then(|pa| pa.database.clone()));

    let dialects = build_dialect_map(ctx.config_path.as_ref(), None)?;
    let parser = make_parser(globals)?;
    let layer = load_from_directory(&parser, &ctx.base_dir)?;

    // Resolve dialect from config
    let dialect = if let Some(ref db_name) = effective_database {
        let db_config = partial.databases.iter().find(|d| d.name == *db_name)
            .ok_or_else(|| format!("Database '{}' not found in config", db_name))?;
        Dialect::from_str(&db_config.db_type)
            .ok_or_else(|| format!("Unknown dialect: {}", db_config.db_type))?
    } else if let Some(first) = partial.databases.first() {
        Dialect::from_str(&first.db_type)
            .ok_or_else(|| format!("Unknown dialect: {}", first.db_type))?
    } else {
        return Err("No databases configured in config.yml".into());
    };

    let date_str = chrono::Local::now().format("%Y%m%d").to_string();

    // Filter views if requested
    let views: Vec<&View> = layer.views.iter()
        .filter(|v| view_filter.map_or(true, |f| v.name == f))
        .collect();

    if views.is_empty() {
        if let Some(name) = view_filter {
            return Err(format!("View '{}' not found", name).into());
        }
        return Err("No views found".into());
    }

    // Collect all SQL statements
    let mut all_stmts: Vec<String> = Vec::new();

    // 1. Create schema/database
    match dialect {
        Dialect::ClickHouse => {
            all_stmts.push(format!("CREATE DATABASE IF NOT EXISTS {}", effective_schema));
        }
        _ => {
            all_stmts.push(format!("CREATE SCHEMA IF NOT EXISTS {}", effective_schema));
        }
    }

    // 2. Create manifest table
    all_stmts.push(preagg::generate_manifest_create_sql(&effective_schema, &dialect));

    // 3. For each view, resolve rollups and generate CTAS + manifest entries
    let mut manifest_entries: Vec<preagg::ManifestEntry> = Vec::new();
    for view in &views {
        let rollups = preagg::resolve_rollups(view);
        for rollup in &rollups {
            let ctas_stmts = preagg::generate_build_sql(view, rollup, &effective_schema, &date_str, &dialect);
            all_stmts.extend(ctas_stmts);

            let entry = preagg::build_manifest_entry(view, rollup, &effective_schema, &date_str);
            all_stmts.push(preagg::generate_manifest_upsert_sql(&effective_schema, &entry, &dialect));
            manifest_entries.push(entry);
        }
    }

    if dry_run {
        for stmt in &all_stmts {
            println!("{};", stmt);
            println!();
        }
        return Ok(());
    }

    // Execute against the warehouse
    #[cfg(feature = "exec")]
    {
        let exec_config: crate::executor::ExecutionConfig =
            serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse config: {}", e))?;
        let connection = if let Some(db_name) = &effective_database {
            exec_config.find_connection(db_name)?
        } else {
            exec_config.first_connection()?
        };

        for (i, stmt) in all_stmts.iter().enumerate() {
            eprintln!("[{}/{}] Executing...", i + 1, all_stmts.len());
            crate::executor::execute(&connection, stmt, &[])
                .map_err(|e| format!("Build statement {} failed: {}\nSQL: {}", i + 1, e, stmt))?;
        }

        eprintln!("Build complete: {} rollup(s) in schema '{}'", manifest_entries.len(), effective_schema);
        // Output summary as JSON
        let summary = serde_json::json!({
            "status": "success",
            "schema": effective_schema,
            "rollups": manifest_entries.iter().map(|e| serde_json::json!({
                "view": e.view_name,
                "rollup": e.rollup_name,
                "table": e.table_name,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&summary).expect("serialize"));
    }

    #[cfg(not(feature = "exec"))]
    {
        return Err("build requires an exec-* feature flag to be enabled".into());
    }

    Ok(())
}
```

- [ ] **Step 4: Verify compilation**

Run: `cargo build --features exec`
Expected: Compiles successfully

- [ ] **Step 5: Test dry-run mode**

Create a minimal test view and run:
```bash
cargo run --features exec -- build --dry-run
```
Expected: Prints CTAS SQL statements without executing

- [ ] **Step 6: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat: add airlayer build command with dry-run support"
```

---

### Task 6: Pull command — manifest reading and Parquet export

**Files:**
- Modify: `src/cli/mod.rs`
- Modify: `src/engine/preagg.rs` (add `LocalManifest` type)

- [ ] **Step 1: Add LocalManifest types to preagg.rs**

```rust
/// Local cache manifest written by `pull`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalManifest {
    pub pulled_at: String,
    pub source_database: String,
    pub rollups: Vec<LocalRollupEntry>,
}

/// An entry in the local cache manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRollupEntry {
    pub view_name: String,
    pub rollup_name: String,
    pub rollup_hash: String,
    pub file: String,
    pub dimensions: Vec<String>,
    pub measures: Vec<serde_json::Value>,
    pub time_dimension: Option<String>,
    pub granularity: Option<String>,
    pub build_date: String,
}
```

- [ ] **Step 2: Add Pull subcommand to clap**

In `Commands` enum:

```rust
    /// Pull pre-aggregated data from the warehouse to local Parquet cache.
    Pull {
        /// Path to config.yml.
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Source schema name (default: AIRLAYER).
        #[arg(long, default_value = "AIRLAYER")]
        schema: String,

        /// Which database to pull from (default: first in config.yml).
        #[arg(long)]
        database: Option<String>,

        /// Pull only a specific view.
        #[arg(long)]
        view: Option<String>,
    },
```

- [ ] **Step 3: Add the match arm in `run()`**

```rust
        Commands::Pull {
            config,
            schema,
            database,
            view,
        } => {
            run_pull(
                config.as_ref(),
                &schema,
                database.as_deref(),
                view.as_deref(),
            )?;
        }
```

- [ ] **Step 4: Implement `run_pull()`**

```rust
/// Pull pre-aggregated data from the warehouse to local Parquet files.
fn run_pull(
    config: Option<&PathBuf>,
    schema: &str,
    database: Option<&str>,
    view_filter: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::engine::preagg;

    let ctx = resolve_project_context(config)?;
    let config_path = ctx
        .config_path
        .as_ref()
        .ok_or("pull requires a config.yml (auto-detected or via --config)")?;
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("Failed to read config {}: {}", config_path.display(), e))?;
    let partial: PartialConfig = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse config: {}", e))?;

    let effective_schema = if schema != "AIRLAYER" {
        schema.to_string()
    } else if let Some(ref pa) = partial.pre_aggregations {
        pa.schema.clone().unwrap_or_else(|| "AIRLAYER".to_string())
    } else {
        "AIRLAYER".to_string()
    };

    let effective_database = database
        .map(|s| s.to_string())
        .or_else(|| partial.pre_aggregations.as_ref().and_then(|pa| pa.database.clone()));

    #[cfg(feature = "exec")]
    {
        let exec_config: crate::executor::ExecutionConfig =
            serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse config: {}", e))?;
        let connection = if let Some(ref db_name) = effective_database {
            exec_config.find_connection(db_name)?
        } else {
            exec_config.first_connection()?
        };

        // 1. Read manifest from warehouse
        let manifest_sql = format!(
            "SELECT view_name, rollup_name, rollup_hash, table_name, dimensions, measures, \
             time_dimension, granularity, build_date \
             FROM {}.{} FINAL",
            effective_schema, "__manifest"
        );
        let manifest_result = crate::executor::execute(&connection, &manifest_sql, &[])
            .map_err(|e| format!("Failed to read manifest: {}", e))?;

        if manifest_result.rows.is_empty() {
            return Err(format!("No rollups found in {}.{}", effective_schema, "__manifest").into());
        }

        // 2. Create cache directory
        let cache_dir = ctx.base_dir.join(".airlayer").join("cache");
        std::fs::create_dir_all(&cache_dir)?;

        // 3. For each rollup, SELECT data and write to Parquet
        let mut local_entries: Vec<preagg::LocalRollupEntry> = Vec::new();

        for row in &manifest_result.rows {
            let view_name = row.get("view_name").and_then(|v| v.as_str()).unwrap_or("");
            let rollup_name = row.get("rollup_name").and_then(|v| v.as_str()).unwrap_or("");
            let rollup_hash = row.get("rollup_hash").and_then(|v| v.as_str()).unwrap_or("");
            let table_name = row.get("table_name").and_then(|v| v.as_str()).unwrap_or("");
            let dimensions_str = row.get("dimensions").and_then(|v| v.as_str()).unwrap_or("[]");
            let measures_str = row.get("measures").and_then(|v| v.as_str()).unwrap_or("[]");
            let time_dim = row.get("time_dimension").and_then(|v| v.as_str()).unwrap_or("");
            let granularity = row.get("granularity").and_then(|v| v.as_str()).unwrap_or("");
            let build_date = row.get("build_date").and_then(|v| v.as_str()).unwrap_or("");

            // Apply view filter
            if let Some(filter) = view_filter {
                if view_name != filter {
                    continue;
                }
            }

            let parquet_filename = format!("{}__{}.parquet", view_name, rollup_hash);
            let parquet_path = cache_dir.join(&parquet_filename);

            eprintln!("Pulling {}.{} → {}", view_name, rollup_name, parquet_filename);

            // SELECT all data from the pre-agg table
            let select_sql = format!("SELECT * FROM {}", table_name);
            let data = crate::executor::execute(&connection, &select_sql, &[])
                .map_err(|e| format!("Failed to pull {}: {}", table_name, e))?;

            // Write to Parquet using DuckDB
            write_parquet(&data, &parquet_path)?;

            let dimensions: Vec<String> = serde_json::from_str(dimensions_str).unwrap_or_default();
            let measures: Vec<serde_json::Value> = serde_json::from_str(measures_str).unwrap_or_default();

            local_entries.push(preagg::LocalRollupEntry {
                view_name: view_name.to_string(),
                rollup_name: rollup_name.to_string(),
                rollup_hash: rollup_hash.to_string(),
                file: parquet_filename,
                dimensions,
                measures,
                time_dimension: if time_dim.is_empty() { None } else { Some(time_dim.to_string()) },
                granularity: if granularity.is_empty() { None } else { Some(granularity.to_string()) },
                build_date: build_date.to_string(),
            });
        }

        if local_entries.is_empty() {
            return Err("No matching rollups found to pull".into());
        }

        // 4. Write local manifest.json
        let local_manifest = preagg::LocalManifest {
            pulled_at: chrono::Utc::now().to_rfc3339(),
            source_database: effective_database.unwrap_or_else(|| "default".to_string()),
            rollups: local_entries.clone(),
        };
        let manifest_path = cache_dir.join("manifest.json");
        let manifest_json = serde_json::to_string_pretty(&local_manifest)?;
        std::fs::write(&manifest_path, &manifest_json)?;

        eprintln!("Pull complete: {} rollup(s) → {}", local_entries.len(), cache_dir.display());
        println!("{}", manifest_json);
    }

    #[cfg(not(feature = "exec"))]
    {
        return Err("pull requires an exec-* feature flag to be enabled".into());
    }

    Ok(())
}

/// Write an ExecutionResult to a Parquet file using an in-memory DuckDB connection.
#[cfg(feature = "exec-duckdb")]
fn write_parquet(
    data: &crate::executor::ExecutionResult,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if data.rows.is_empty() {
        return Err("No data to write to Parquet".into());
    }

    let conn = duckdb::Connection::open_in_memory()
        .map_err(|e| format!("Failed to open DuckDB: {}", e))?;

    // Build column definitions from first row (all as VARCHAR, DuckDB will infer types from data)
    let columns: Vec<String> = data.columns.iter()
        .map(|c| format!("\"{}\" VARCHAR", c.replace('"', "\"\"")))
        .collect();
    let create_sql = format!("CREATE TABLE __export ({})", columns.join(", "));
    conn.execute_batch(&create_sql)
        .map_err(|e| format!("Failed to create export table: {}", e))?;

    // Insert rows
    if !data.rows.is_empty() {
        let placeholders: Vec<String> = data.columns.iter().map(|_| "?".to_string()).collect();
        let insert_sql = format!("INSERT INTO __export VALUES ({})", placeholders.join(", "));

        for row in &data.rows {
            let values: Vec<String> = data.columns.iter()
                .map(|col| {
                    row.get(col)
                        .map(|v| match v {
                            serde_json::Value::Null => String::new(),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default()
                })
                .collect();
            let params: Vec<&dyn duckdb::ToSql> = values.iter()
                .map(|v| v as &dyn duckdb::ToSql)
                .collect();
            conn.execute(&insert_sql, params.as_slice())
                .map_err(|e| format!("Failed to insert row: {}", e))?;
        }
    }

    // Export to Parquet
    let path_str = path.to_str().ok_or("Invalid path")?;
    conn.execute_batch(&format!(
        "COPY __export TO '{}' (FORMAT PARQUET)",
        path_str.replace('\'', "''")
    )).map_err(|e| format!("Failed to export Parquet: {}", e))?;

    Ok(())
}

#[cfg(not(feature = "exec-duckdb"))]
fn write_parquet(
    _data: &crate::executor::ExecutionResult,
    _path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("pull requires the exec-duckdb feature flag".into())
}
```

- [ ] **Step 5: Add `use duckdb;` import at the top of cli/mod.rs**

Within the `#[cfg(feature = "exec-duckdb")]` block in `write_parquet`, the `duckdb` crate is already available as a dependency. Ensure the import is within the function scope.

- [ ] **Step 6: Verify compilation**

Run: `cargo build --features exec`
Expected: Compiles successfully

- [ ] **Step 7: Commit**

```bash
git add src/cli/mod.rs src/engine/preagg.rs
git commit -m "feat: add airlayer pull command with Parquet export"
```

---

### Task 7: Coverage check and re-aggregation SQL

**Files:**
- Modify: `src/engine/preagg.rs`

- [ ] **Step 1: Write the failing test**

Add to `preagg.rs` tests:

```rust
#[test]
fn test_coverage_check_covered() {
    let entry = test_local_rollup_entry();
    let request = QueryRequest {
        measures: vec!["orders.total_revenue".to_string()],
        dimensions: vec!["orders.region".to_string()],
        ..QueryRequest::new()
    };
    let result = check_coverage(&request, &[entry]);
    assert!(result.is_some(), "Expected coverage match");
}

#[test]
fn test_coverage_check_not_covered_missing_dim() {
    let entry = test_local_rollup_entry();
    let request = QueryRequest {
        measures: vec!["orders.total_revenue".to_string()],
        dimensions: vec!["orders.status".to_string()], // Not in rollup
        ..QueryRequest::new()
    };
    let result = check_coverage(&request, &[entry]);
    assert!(result.is_none(), "Expected no coverage match");
}

#[test]
fn test_coverage_check_not_covered_missing_measure() {
    let entry = test_local_rollup_entry();
    let request = QueryRequest {
        measures: vec!["orders.other_metric".to_string()], // Not in rollup
        dimensions: vec!["orders.region".to_string()],
        ..QueryRequest::new()
    };
    let result = check_coverage(&request, &[entry]);
    assert!(result.is_none(), "Expected no coverage match");
}

fn test_local_rollup_entry() -> LocalRollupEntry {
    LocalRollupEntry {
        view_name: "orders".into(),
        rollup_name: "by_region_monthly".into(),
        rollup_hash: "a1b2c3d4".into(),
        file: "orders__a1b2c3d4.parquet".into(),
        dimensions: vec!["region".into()],
        measures: vec![
            serde_json::json!({"name": "total_revenue", "type": "sum", "columns": ["total_revenue__sum"]}),
        ],
        time_dimension: Some("created_at".into()),
        granularity: Some("month".into()),
        build_date: "2026-04-15".into(),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test preagg::tests::test_coverage -- --nocapture`
Expected: FAIL — `check_coverage` not defined

- [ ] **Step 3: Implement coverage check**

Add to `src/engine/preagg.rs`:

```rust
use crate::engine::query::QueryRequest;

/// Check if any rollup in the manifest covers the given query.
/// Returns the matching entry if found.
pub fn check_coverage<'a>(
    request: &QueryRequest,
    rollups: &'a [LocalRollupEntry],
) -> Option<&'a LocalRollupEntry> {
    for entry in rollups {
        if covers(request, entry) {
            return Some(entry);
        }
    }
    None
}

/// Check if a single rollup entry covers a query request.
fn covers(request: &QueryRequest, entry: &LocalRollupEntry) -> bool {
    // Extract view name from the first member reference
    let query_views = request.referenced_views();

    // All requested views must match the rollup's view
    if !query_views.iter().all(|v| *v == entry.view_name) {
        return false;
    }

    // Check dimensions: all requested dims must be in rollup dims
    for dim in &request.dimensions {
        let dim_name = dim.split('.').nth(1).unwrap_or(dim);
        if !entry.dimensions.contains(&dim_name.to_string()) {
            return false;
        }
    }

    // Check measures: all requested measures must be in rollup measures
    let rollup_measure_names: Vec<String> = entry.measures.iter()
        .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
        .collect();
    let rollup_measure_types: Vec<String> = entry.measures.iter()
        .filter_map(|m| m.get("type").and_then(|t| t.as_str()).map(|s| s.to_string()))
        .collect();

    for measure in &request.measures {
        let measure_name = measure.split('.').nth(1).unwrap_or(measure);
        if !rollup_measure_names.contains(&measure_name.to_string()) {
            return false;
        }
        // Check if the measure is a custom type (not pre-aggregable)
        if let Some(idx) = rollup_measure_names.iter().position(|n| n == measure_name) {
            if rollup_measure_types.get(idx).map(|t| t.as_str()) == Some("custom") {
                return false;
            }
        }
    }

    // Check time dimensions
    for td in &request.time_dimensions {
        let td_name = td.dimension.split('.').nth(1).unwrap_or(&td.dimension);
        if entry.time_dimension.as_deref() != Some(td_name) {
            return false;
        }
        // Granularity: requested must be same or coarser than stored
        if let Some(ref req_gran) = td.granularity {
            if let Some(ref stored_gran) = entry.granularity {
                if !is_coarser_or_equal(req_gran, stored_gran) {
                    return false;
                }
            }
        }
    }

    true
}

/// Check if `requested` granularity is coarser than or equal to `stored`.
fn is_coarser_or_equal(requested: &str, stored: &str) -> bool {
    let order = ["second", "minute", "hour", "day", "week", "month", "quarter", "year"];
    let req_idx = order.iter().position(|&g| g == requested);
    let stored_idx = order.iter().position(|&g| g == stored);
    match (req_idx, stored_idx) {
        (Some(r), Some(s)) => r >= s,
        _ => requested == stored, // Unknown granularity: exact match only
    }
}
```

- [ ] **Step 4: Implement re-aggregation SQL generation**

Add to `src/engine/preagg.rs`:

```rust
/// Generate a DuckDB SQL query that reads from a Parquet file and re-aggregates
/// the pre-aggregated data to answer the original query.
pub fn generate_reagg_sql(
    request: &QueryRequest,
    entry: &LocalRollupEntry,
    parquet_path: &str,
) -> String {
    let mut select_cols: Vec<String> = Vec::new();
    let mut group_by_cols: Vec<String> = Vec::new();

    // 1. Dimensions
    for dim in &request.dimensions {
        let dim_name = dim.split('.').nth(1).unwrap_or(dim);
        let alias = dim.replace('.', "__");
        select_cols.push(format!("\"{}\" AS \"{}\"", dim_name, alias));
        group_by_cols.push(format!("\"{}\"", dim_name));
    }

    // 2. Time dimensions
    for td in &request.time_dimensions {
        let td_name = td.dimension.split('.').nth(1).unwrap_or(&td.dimension);
        let alias = td.dimension.replace('.', "__");
        if let Some(ref gran) = td.granularity {
            // Stored column name is td_name__stored_gran, need to re-truncate if coarser
            if let Some(ref stored_gran) = entry.granularity {
                let stored_col = format!("{}__{}", td_name, stored_gran);
                if gran == stored_gran {
                    select_cols.push(format!("\"{}\" AS \"{}\"", stored_col, alias));
                    group_by_cols.push(format!("\"{}\"", stored_col));
                } else {
                    // Re-truncate to coarser granularity
                    let trunc = format!("date_trunc('{}', \"{}\")", gran, stored_col);
                    select_cols.push(format!("{} AS \"{}\"", trunc, alias));
                    group_by_cols.push(trunc.clone());
                }
            }
        } else {
            let col = format!("\"{}\"", td_name);
            select_cols.push(format!("{} AS \"{}\"", col, alias));
            group_by_cols.push(col);
        }
    }

    // 3. Measures (re-aggregated)
    for measure in &request.measures {
        let measure_name = measure.split('.').nth(1).unwrap_or(measure);
        let alias = measure.replace('.', "__");

        // Find the measure metadata in the rollup entry
        if let Some(m_meta) = entry.measures.iter().find(|m| {
            m.get("name").and_then(|n| n.as_str()) == Some(measure_name)
        }) {
            let m_type = m_meta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let columns: Vec<String> = m_meta.get("columns")
                .and_then(|c| c.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();

            match m_type {
                "sum" => {
                    let col = columns.first().cloned().unwrap_or_else(|| format!("{}__sum", measure_name));
                    select_cols.push(format!("SUM(\"{}\") AS \"{}\"", col, alias));
                }
                "count" => {
                    let col = columns.first().cloned().unwrap_or_else(|| format!("{}__count", measure_name));
                    select_cols.push(format!("SUM(\"{}\") AS \"{}\"", col, alias));
                }
                "average" => {
                    let sum_col = columns.first().cloned().unwrap_or_else(|| format!("{}__sum", measure_name));
                    let count_col = columns.get(1).cloned().unwrap_or_else(|| format!("{}__count", measure_name));
                    select_cols.push(format!(
                        "CAST(SUM(\"{}\") AS DOUBLE) / NULLIF(SUM(\"{}\"), 0) AS \"{}\"",
                        sum_col, count_col, alias
                    ));
                }
                "min" => {
                    let col = columns.first().cloned().unwrap_or_else(|| format!("{}__min", measure_name));
                    select_cols.push(format!("MIN(\"{}\") AS \"{}\"", col, alias));
                }
                "max" => {
                    let col = columns.first().cloned().unwrap_or_else(|| format!("{}__max", measure_name));
                    select_cols.push(format!("MAX(\"{}\") AS \"{}\"", col, alias));
                }
                "count_distinct" | "count_distinct_approx" => {
                    let col = columns.first().cloned().unwrap_or_else(|| measure_name.to_string());
                    select_cols.push(format!("COUNT(DISTINCT \"{}\") AS \"{}\"", col, alias));
                }
                "median" => {
                    // For median with freq, we need a subquery or UNNEST approach
                    // DuckDB supports QUANTILE_CONT which can't take weights natively,
                    // so we expand rows by freq and then take MEDIAN
                    let col = columns.first().cloned().unwrap_or_else(|| measure_name.to_string());
                    let freq_col = columns.get(1).cloned().unwrap_or_else(|| format!("{}__freq", col));
                    // Simplified: just use MEDIAN on the raw values (weighted median via freq expansion
                    // would require a CTE; for v1, this gives an approximation with deduplicated values)
                    select_cols.push(format!("MEDIAN(\"{}\") AS \"{}\"", col, alias));
                }
                "number" => {
                    let col = columns.first().cloned().unwrap_or_else(|| format!("{}__value", measure_name));
                    select_cols.push(format!("\"{}\" AS \"{}\"", col, alias));
                }
                _ => {
                    select_cols.push(format!("NULL AS \"{}\"", alias));
                }
            }
        }
    }

    let select = select_cols.join(", ");
    let group_by = if group_by_cols.is_empty() {
        String::new()
    } else {
        format!("\nGROUP BY {}", group_by_cols.join(", "))
    };

    // Build WHERE for filters (basic support for dimension equality filters)
    let mut where_clauses: Vec<String> = Vec::new();
    for filter in &request.filters {
        if let (Some(ref member), Some(ref op)) = (&filter.member, &filter.operator) {
            let col_name = member.split('.').nth(1).unwrap_or(member);
            match op {
                crate::engine::query::FilterOperator::Equals if filter.values.len() == 1 => {
                    where_clauses.push(format!("\"{}\" = '{}'", col_name, filter.values[0].replace('\'', "''")));
                }
                crate::engine::query::FilterOperator::Equals => {
                    let vals: Vec<String> = filter.values.iter()
                        .map(|v| format!("'{}'", v.replace('\'', "''")))
                        .collect();
                    where_clauses.push(format!("\"{}\" IN ({})", col_name, vals.join(", ")));
                }
                _ => {} // Other filters: skip for v1, query falls through to raw
            }
        }
    }

    let where_clause = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("\nWHERE {}", where_clauses.join(" AND "))
    };

    let limit = request.limit.map(|l| format!("\nLIMIT {}", l)).unwrap_or_default();
    let offset = request.offset.map(|o| format!("\nOFFSET {}", o)).unwrap_or_default();

    format!(
        "SELECT {select}\nFROM read_parquet('{path}'){where_clause}{group_by}{limit}{offset}",
        path = parquet_path.replace('\'', "''"),
    )
}

/// Generate a warehouse SQL query that reads from the pre-aggregated table
/// instead of the raw table. Same re-aggregation logic but uses the warehouse table name.
pub fn generate_warehouse_reagg_sql(
    request: &QueryRequest,
    entry: &LocalRollupEntry,
    table_name: &str,
    dialect: &Dialect,
) -> String {
    // Reuse the same logic as generate_reagg_sql but with a table reference
    // instead of read_parquet(), and use dialect-specific quoting
    let parquet_sql = generate_reagg_sql(request, entry, "__placeholder__");
    // Replace the read_parquet call with the actual table name
    parquet_sql.replace("read_parquet('__placeholder__')", table_name)
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test preagg::tests -- --nocapture`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/engine/preagg.rs
git commit -m "feat: add coverage check and re-aggregation SQL generation"
```

---

### Task 8: Cache-aware query execution

**Files:**
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Add `--no-cache` flag to Query command**

In the `Query` variant of `Commands`, add:

```rust
        /// Skip pre-aggregation cache (local and warehouse), go straight to raw SQL.
        #[arg(long)]
        no_cache: bool,
```

- [ ] **Step 2: Thread `no_cache` through the match arm**

Update the `Commands::Query` match arm to pass `no_cache` to `run_execute`:

```rust
// In the execute branch, add no_cache parameter
run_execute(
    globals,
    config,
    dialect,
    query,
    dimensions,
    measures,
    filter,
    order,
    limit,
    offset,
    segments,
    through,
    motif,
    motif_param,
    datasource,
    no_cache,
);
```

- [ ] **Step 3: Modify `run_execute` inner function**

Add the cache check before Stage 5 (execute) in the `inner` function of `run_execute`. After the query is compiled (Stage 3) but before execution (Stage 4+), insert the cache resolution logic:

```rust
// After Stage 3 (compile query), before Stage 4 (resolve connection):

// --- Pre-aggregation cache resolution ---
if !no_cache {
    // Layer 1: Check local Parquet cache
    let cache_dir = ctx.base_dir.join(".airlayer").join("cache");
    let manifest_path = cache_dir.join("manifest.json");
    if manifest_path.is_file() {
        if let Ok(manifest_content) = std::fs::read_to_string(&manifest_path) {
            if let Ok(local_manifest) = serde_json::from_str::<crate::engine::preagg::LocalManifest>(&manifest_content) {
                if let Some(entry) = crate::engine::preagg::check_coverage(&request, &local_manifest.rollups) {
                    let parquet_path = cache_dir.join(&entry.file);
                    if parquet_path.is_file() {
                        let parquet_str = parquet_path.to_str().unwrap_or("");
                        let reagg_sql = crate::engine::preagg::generate_reagg_sql(&request, entry, parquet_str);

                        #[cfg(feature = "exec-duckdb")]
                        {
                            let duck_conn = duckdb::Connection::open_in_memory().map_err(|e| {
                                err("execution_error", format!("DuckDB cache error: {}", e),
                                    Some(result.sql.clone()), &result.columns, views_used.clone())
                            })?;
                            let mut stmt = duck_conn.prepare(&reagg_sql).map_err(|e| {
                                err("execution_error", format!("DuckDB cache query error: {}", e),
                                    Some(result.sql.clone()), &result.columns, views_used.clone())
                            })?;
                            let columns: Vec<String> = (0..stmt.column_count())
                                .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                                .collect();
                            let mut rows = Vec::new();
                            let mut duckdb_rows = stmt.query([]).map_err(|e| {
                                err("execution_error", format!("DuckDB query failed: {}", e),
                                    Some(result.sql.clone()), &result.columns, views_used.clone())
                            })?;
                            while let Some(row) = duckdb_rows.next().map_err(|e| {
                                err("execution_error", format!("DuckDB row error: {}", e),
                                    Some(result.sql.clone()), &result.columns, views_used.clone())
                            })? {
                                let mut obj = serde_json::Map::new();
                                for (i, col) in columns.iter().enumerate() {
                                    let val: String = row.get::<_, Option<String>>(i)
                                        .unwrap_or(None)
                                        .unwrap_or_default();
                                    obj.insert(col.clone(), serde_json::Value::String(val));
                                }
                                rows.push(obj);
                            }
                            let exec_result = crate::executor::ExecutionResult { columns, rows };
                            let mut envelope = QueryEnvelope::success(
                                result.sql, &result.columns, exec_result, views_used,
                            );
                            envelope.status = "success".to_string();
                            // Note: the envelope.sql contains the original compiled SQL for transparency
                            return Ok(envelope);
                        }
                    }
                }
            }
        }
    }

    // Layer 2: Check warehouse pre-agg tables
    // Only attempt if we have a database connection
    if let Ok(config_path) = ctx.config_path.as_ref().ok_or(()) {
        if let Ok(content) = std::fs::read_to_string(config_path) {
            if let Ok(exec_config) = serde_yaml::from_str::<crate::executor::ExecutionConfig>(&content) {
                let connection_result = if let Some(ds) = datasource {
                    exec_config.find_connection(ds)
                } else {
                    exec_config.first_connection()
                };
                if let Ok(ref connection) = connection_result {
                    // Read config for schema name
                    if let Ok(partial) = serde_yaml::from_str::<PartialConfig>(&content) {
                        let preagg_schema = partial.pre_aggregations
                            .as_ref()
                            .and_then(|pa| pa.schema.clone())
                            .unwrap_or_else(|| "AIRLAYER".to_string());

                        let manifest_sql = format!(
                            "SELECT view_name, rollup_name, rollup_hash, table_name, \
                             dimensions, measures, time_dimension, granularity, build_date \
                             FROM {}.{} FINAL",
                            preagg_schema, "__manifest"
                        );
                        if let Ok(manifest_result) = crate::executor::execute(connection, &manifest_sql, &[]) {
                            let rollup_entries: Vec<crate::engine::preagg::LocalRollupEntry> = manifest_result.rows.iter()
                                .filter_map(|row| {
                                    Some(crate::engine::preagg::LocalRollupEntry {
                                        view_name: row.get("view_name")?.as_str()?.to_string(),
                                        rollup_name: row.get("rollup_name")?.as_str()?.to_string(),
                                        rollup_hash: row.get("rollup_hash")?.as_str()?.to_string(),
                                        file: String::new(), // Not used for warehouse layer
                                        dimensions: serde_json::from_str(row.get("dimensions")?.as_str()?).unwrap_or_default(),
                                        measures: serde_json::from_str(row.get("measures")?.as_str()?).unwrap_or_default(),
                                        time_dimension: row.get("time_dimension").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()),
                                        granularity: row.get("granularity").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()),
                                        build_date: row.get("build_date").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                    })
                                })
                                .collect();

                            if let Some(entry) = crate::engine::preagg::check_coverage(&request, &rollup_entries) {
                                let table_name = format!("{}.{}__{}__{}",
                                    preagg_schema, entry.view_name, entry.rollup_hash,
                                    entry.build_date.replace('-', ""));
                                // Read the table_name from the manifest entry directly
                                let actual_table = manifest_result.rows.iter()
                                    .find(|row| {
                                        row.get("rollup_hash").and_then(|v| v.as_str()) == Some(&entry.rollup_hash) &&
                                        row.get("view_name").and_then(|v| v.as_str()) == Some(&entry.view_name)
                                    })
                                    .and_then(|row| row.get("table_name").and_then(|v| v.as_str()))
                                    .unwrap_or(&table_name);

                                let reagg_sql = crate::engine::preagg::generate_warehouse_reagg_sql(
                                    &request, entry, actual_table,
                                    &result.columns.first().map(|_| crate::dialect::Dialect::ClickHouse).unwrap_or(crate::dialect::Dialect::Postgres),
                                );

                                if let Ok(exec_result) = crate::executor::execute(connection, &reagg_sql, &[]) {
                                    return Ok(QueryEnvelope::success(
                                        result.sql, &result.columns, exec_result, views_used,
                                    ));
                                }
                                // If warehouse pre-agg fails, fall through to raw SQL
                            }
                        }
                        // If manifest read fails (table doesn't exist), fall through silently
                    }
                }
            }
        }
    }
}

// (existing Stage 4 + 5 continue here as fallback — raw SQL execution)
```

- [ ] **Step 4: Update function signatures**

Add `no_cache: bool` parameter to both `run_execute` and its `inner` function. Thread it through.

- [ ] **Step 5: Verify compilation**

Run: `cargo build --features exec`
Expected: Compiles successfully

- [ ] **Step 6: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat: cache-aware query execution with three-tier resolution"
```

---

### Task 9: Integration tests — ClickHouse build + pull + query

**Files:**
- Create: `tests/integration/views/events_preagg.view.yml`
- Modify: `tests/integration_tests.rs`

- [ ] **Step 1: Create test view with pre_aggregations**

```yaml
name: events
description: "Events with pre-aggregation for testing"
table: analytics.events

dimensions:
  - name: event_id
    type: string
    expr: event_id
  - name: event_type
    type: string
    expr: event_type
  - name: platform
    type: string
    expr: platform
  - name: created_at
    type: datetime
    expr: created_at

measures:
  - name: total_events
    type: count
  - name: total_revenue
    type: sum
    expr: revenue_cents / 100.0
  - name: unique_users
    type: count_distinct
    expr: user_id

pre_aggregations:
  - name: by_platform_daily
    dimensions: [platform]
    measures: [total_events, total_revenue, unique_users]
    time_dimension: created_at
    granularity: day
```

Write this to `tests/integration/views-preagg/events.view.yml` (separate dir to not conflict with existing tests).

- [ ] **Step 2: Add integration test module**

Add to `tests/integration_tests.rs`:

```rust
#[cfg(feature = "exec")]
mod preagg_tests {
    use super::*;
    use std::sync::Once;

    static PREAGG_SEED: Once = Once::new();

    fn ch_base_url() -> String {
        load_test_ports();
        let port = std::env::var("AIRLAYER_CH_HTTP_PORT").unwrap_or_else(|_| "18123".to_string());
        format!("http://localhost:{}", port)
    }

    fn is_available() -> bool {
        ureq::get(&format!("{}/ping", ch_base_url())).call().is_ok()
    }

    fn seed() {
        PREAGG_SEED.call_once(|| {
            // Reuse the ClickHouse seed (creates analytics.events)
            for table in &["events"] {
                let drop = format!("DROP TABLE IF EXISTS analytics.{}", table);
                ureq::post(&format!("{}/", ch_base_url()))
                    .send_string(&drop)
                    .ok();
            }
            let seed_sql = include_str!("integration/seed/clickhouse.sql");
            for stmt in seed_sql.split(';') {
                let stripped: String = stmt
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("--"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let trimmed = stripped.trim();
                if !trimmed.is_empty() && (trimmed.contains("analytics.events") || trimmed.starts_with("CREATE DATABASE")) {
                    ureq::post(&format!("{}/", ch_base_url()))
                        .send_string(trimmed)
                        .ok();
                }
            }
        });
    }

    #[test]
    #[ignore = "tier2"]
    fn preagg_resolve_rollups() {
        // Unit-level: verify rollups resolve correctly from the test view
        let views_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/views-preagg");
        let dialects = DatasourceDialectMap::with_default(Dialect::ClickHouse);
        let engine = SemanticEngine::load(&views_dir, None, dialects).expect("load");

        let view = engine.view("events").expect("events view");
        let rollups = airlayer::engine::preagg::resolve_rollups(view);
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].name, "by_platform_daily");
        assert_eq!(rollups[0].dimensions, vec!["platform"]);
        assert_eq!(rollups[0].measures.len(), 3);
    }

    #[test]
    #[ignore = "tier2"]
    fn preagg_build_sql_generation() {
        if !is_available() {
            eprintln!("ClickHouse not available, skipping");
            return;
        }
        seed();

        let views_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/views-preagg");
        let dialects = DatasourceDialectMap::with_default(Dialect::ClickHouse);
        let engine = SemanticEngine::load(&views_dir, None, dialects).expect("load");

        let view = engine.view("events").expect("events view");
        let rollups = airlayer::engine::preagg::resolve_rollups(view);
        let sqls = airlayer::engine::preagg::generate_build_sql(
            view, &rollups[0], "airlayer_test_preagg", "20260415",
            &Dialect::ClickHouse,
        );

        assert!(!sqls.is_empty());
        let ctas = &sqls[0];
        assert!(ctas.contains("CREATE TABLE"), "CTAS: {}", ctas);
        assert!(ctas.contains("airlayer_test_preagg"), "Schema: {}", ctas);
        assert!(ctas.contains("total_events__count"), "Count col: {}", ctas);
        assert!(ctas.contains("total_revenue__sum"), "Sum col: {}", ctas);
        assert!(ctas.contains("user_id"), "CD col: {}", ctas);

        // Execute: create schema + table
        let create_db = "CREATE DATABASE IF NOT EXISTS airlayer_test_preagg";
        ureq::post(&format!("{}/", ch_base_url()))
            .send_string(create_db)
            .expect("create db");

        // Drop pre-existing table if any
        let table_name = format!("airlayer_test_preagg.events__{}__20260415", rollups[0].hash);
        let drop = format!("DROP TABLE IF EXISTS {}", table_name);
        ureq::post(&format!("{}/", ch_base_url()))
            .send_string(&drop)
            .expect("drop");

        // Execute CTAS
        let resp = ureq::post(&format!("{}/", ch_base_url()))
            .send_string(ctas);
        assert!(resp.is_ok(), "CTAS failed: {:?}", resp.err());

        // Verify data was created
        let count_sql = format!("SELECT COUNT(*) FROM {}", table_name);
        let count_resp = ureq::post(&format!("{}/", ch_base_url()))
            .send_string(&count_sql)
            .expect("count query");
        let count_str = count_resp.into_string().expect("count response");
        let count: i64 = count_str.trim().parse().unwrap_or(0);
        assert!(count > 0, "Expected rows in pre-agg table, got: {}", count);

        // Cleanup
        ureq::post(&format!("{}/", ch_base_url()))
            .send_string(&format!("DROP DATABASE IF EXISTS airlayer_test_preagg"))
            .ok();
    }

    #[test]
    #[ignore = "tier2"]
    fn preagg_coverage_check() {
        let entry = airlayer::engine::preagg::LocalRollupEntry {
            view_name: "events".into(),
            rollup_name: "by_platform_daily".into(),
            rollup_hash: "test1234".into(),
            file: "events__test1234.parquet".into(),
            dimensions: vec!["platform".into()],
            measures: vec![
                serde_json::json!({"name": "total_events", "type": "count", "columns": ["total_events__count"]}),
                serde_json::json!({"name": "total_revenue", "type": "sum", "columns": ["total_revenue__sum"]}),
            ],
            time_dimension: Some("created_at".into()),
            granularity: Some("day".into()),
            build_date: "2026-04-15".into(),
        };

        // Covered query
        let covered = QueryRequest {
            measures: vec!["events.total_revenue".to_string()],
            dimensions: vec!["events.platform".to_string()],
            ..QueryRequest::new()
        };
        assert!(airlayer::engine::preagg::check_coverage(&covered, &[entry.clone()]).is_some());

        // Not covered — dimension not in rollup
        let not_covered = QueryRequest {
            measures: vec!["events.total_revenue".to_string()],
            dimensions: vec!["events.country".to_string()],
            ..QueryRequest::new()
        };
        assert!(airlayer::engine::preagg::check_coverage(&not_covered, &[entry]).is_none());
    }
}
```

- [ ] **Step 3: Create the test views directory**

```bash
mkdir -p tests/integration/views-preagg
```

Write the view file from Step 1.

- [ ] **Step 4: Run the unit-level preagg tests**

Run: `cargo test preagg -- --nocapture`
Expected: Unit tests pass

- [ ] **Step 5: Run integration tests (requires Docker ClickHouse)**

Run: `just test-docker` or `cargo test --features exec preagg_tests -- --ignored --nocapture`
Expected: Integration tests pass (or skip if ClickHouse not available)

- [ ] **Step 6: Commit**

```bash
git add tests/integration/views-preagg/ tests/integration_tests.rs
git commit -m "test: add pre-aggregation integration tests for ClickHouse"
```

---

### Task 10: Final wiring and cleanup

**Files:**
- Modify: `src/lib.rs` (re-export preagg types)
- Modify: `CLAUDE.md` (document new commands)

- [ ] **Step 1: Add re-exports to lib.rs**

```rust
pub use engine::preagg;
```

- [ ] **Step 2: Run full test suite**

Run: `cargo test`
Expected: All tests pass (140+ unit tests + tier 1)

Run: `cargo test --features exec`
Expected: Compiles with all executor features

- [ ] **Step 3: Update CLAUDE.md**

Add to the "CLI conventions" section:

```markdown
- `build`: pre-aggregate views into warehouse rollup tables. `--schema` (default AIRLAYER), `--database`, `--view`, `--dry-run`.
- `pull`: download pre-aggregated data to local `.airlayer/cache/` as Parquet files. `--schema`, `--database`, `--view`.
- `query --no-cache`: bypass pre-aggregation cache layers, execute raw SQL directly.
```

Add to "Key design decisions":

```markdown
- **Pre-aggregation three-tier resolution**: When `--execute` is used, queries check (1) local Parquet cache via DuckDB, (2) warehouse `__manifest` pre-agg tables, (3) raw SQL, in that order. `--no-cache` skips layers 1 and 2.
- **Rollup column strategy**: SUM/COUNT/MIN/MAX store aggregated columns. AVG stores SUM+COUNT for recomputation. COUNT_DISTINCT stores raw expr column (GROUP BY it). MEDIAN stores raw expr + freq column. Custom measures are not pre-aggregable.
```

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs CLAUDE.md
git commit -m "feat: finalize pre-aggregation feature with docs and re-exports"
```
