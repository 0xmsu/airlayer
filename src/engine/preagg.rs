//! Pre-aggregation: rollup resolution, SQL generation, coverage checking.

use crate::dialect::Dialect;
use crate::schema::models::{MeasureType, PreAggregation, View};
use serde::{Deserialize, Serialize};

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

/// Generate the CTAS SQL statements for a rollup.
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

    let order_by = group_by.clone();

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

#[cfg(test)]
mod tests {
    use super::*;

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
