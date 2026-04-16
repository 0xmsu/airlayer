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
        .filter(|m| {
            m.measure_type != MeasureType::Custom
                && m.measure_type != MeasureType::Number
                && m.measure_type != MeasureType::Median
        })
        .map(build_rollup_measure)
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
    let table_name = format!("{}__{}__{}", view.name, rollup.hash, date_str);
    let fq_table = dialect.qualify_table(schema, &table_name);

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
    // Track quoted aliases for ClickHouse ORDER BY (needs column names, not positional refs)
    let mut group_by_aliases: Vec<String> = Vec::new();

    // 1. Dimensions
    for dim_name in &rollup.dimensions {
        if let Some(dim) = view.dimensions.iter().find(|d| d.name == *dim_name) {
            let alias = dialect.quote_identifier(dim_name);
            select_cols.push(format!("{} AS {}", dim.expr, alias));
            group_by_cols.push(dim.expr.clone());
            group_by_aliases.push(alias);
        }
    }

    // 2. Time dimension (truncated)
    if let (Some(ref td_name), Some(ref gran)) = (&rollup.time_dimension, &rollup.granularity) {
        if let Some(td) = view.dimensions.iter().find(|d| d.name == *td_name) {
            let trunc_expr = dialect.date_trunc(gran, &td.expr);
            let alias = dialect.quote_identifier(&format!("{}__{}", td_name, gran));
            select_cols.push(format!("{} AS {}", trunc_expr, alias));
            group_by_cols.push(trunc_expr);
            group_by_aliases.push(alias);
        }
    }

    // 3. Extra GROUP BY columns for count_distinct / median
    for col in &extra_group_cols {
        let alias = dialect.quote_identifier(col);
        select_cols.push(format!("{} AS {}", col, alias));
        group_by_cols.push(col.clone());
        group_by_aliases.push(alias);
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

    let ctas = match dialect {
        Dialect::ClickHouse => {
            let order_by = group_by_aliases.join(", ");
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

    let drop = format!("DROP TABLE IF EXISTS {}", fq_table);
    vec![drop, ctas]
}

/// Generate the CREATE TABLE statement for the __manifest table.
pub fn generate_manifest_create_sql(schema: &str, dialect: &Dialect) -> String {
    let fq_table = dialect.qualify_table(schema, "__manifest");
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
        // BigQuery uses STRING, not VARCHAR
        Dialect::BigQuery => format!(
            "CREATE TABLE IF NOT EXISTS {fq_table} (\n\
             \x20   view_name STRING,\n\
             \x20   rollup_name STRING,\n\
             \x20   rollup_hash STRING,\n\
             \x20   table_name STRING,\n\
             \x20   dimensions STRING,\n\
             \x20   measures STRING,\n\
             \x20   time_dimension STRING,\n\
             \x20   granularity STRING,\n\
             \x20   build_date DATE\n\
             )"
        ),
        // SQLite doesn't support composite PRIMARY KEY in column defs
        Dialect::SQLite => format!(
            "CREATE TABLE IF NOT EXISTS {fq_table} (\n\
             \x20   view_name TEXT,\n\
             \x20   rollup_name TEXT,\n\
             \x20   rollup_hash TEXT,\n\
             \x20   table_name TEXT,\n\
             \x20   dimensions TEXT,\n\
             \x20   measures TEXT,\n\
             \x20   time_dimension TEXT,\n\
             \x20   granularity TEXT,\n\
             \x20   build_date TEXT,\n\
             \x20   UNIQUE (view_name, rollup_name)\n\
             )"
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

/// Generate upsert SQL for a manifest entry.
/// ClickHouse uses INSERT (ReplacingMergeTree handles dedup).
/// SQLite uses INSERT OR REPLACE (UNIQUE constraint handles dedup).
/// Other dialects use DELETE + INSERT to handle re-builds.
pub fn generate_manifest_upsert_sql(
    schema: &str,
    entry: &ManifestEntry,
    dialect: &Dialect,
) -> Vec<String> {
    let fq_table = dialect.qualify_table(schema, "__manifest");
    let values = format!(
        "('{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}')",
        entry.view_name.replace('\'', "''"),
        entry.rollup_name.replace('\'', "''"),
        entry.rollup_hash.replace('\'', "''"),
        entry.table_name.replace('\'', "''"),
        serde_json::to_string(&entry.dimensions)
            .unwrap_or_default()
            .replace('\'', "''"),
        entry.measures_json.replace('\'', "''"),
        entry
            .time_dimension
            .as_deref()
            .unwrap_or("")
            .replace('\'', "''"),
        entry
            .granularity
            .as_deref()
            .unwrap_or("")
            .replace('\'', "''"),
        entry.build_date.replace('\'', "''"),
    );
    let columns = "(view_name, rollup_name, rollup_hash, table_name, dimensions, measures, time_dimension, granularity, build_date)";
    match dialect {
        // ClickHouse: ReplacingMergeTree handles dedup, just INSERT
        Dialect::ClickHouse => {
            vec![format!("INSERT INTO {fq_table} {columns} VALUES {values}")]
        }
        // SQLite: use INSERT OR REPLACE (relies on UNIQUE constraint)
        Dialect::SQLite => {
            vec![format!(
                "INSERT OR REPLACE INTO {fq_table} {columns} VALUES {values}"
            )]
        }
        // All others: DELETE + INSERT
        _ => {
            let delete = format!(
                "DELETE FROM {fq_table} WHERE view_name = '{}' AND rollup_name = '{}'",
                entry.view_name.replace('\'', "''"),
                entry.rollup_name.replace('\'', "''"),
            );
            let insert = format!("INSERT INTO {fq_table} {columns} VALUES {values}");
            vec![delete, insert]
        }
    }
}

/// Check if any rollup in the manifest covers the given query.
/// Returns a reference to the first matching entry, or None if no rollup covers the query.
pub fn check_coverage<'a>(
    request: &crate::engine::query::QueryRequest,
    rollups: &'a [LocalRollupEntry],
) -> Option<&'a LocalRollupEntry> {
    rollups.iter().find(|entry| covers(request, entry))
}

/// Recursively collect member names from a filter tree.
fn collect_filter_members(filter: &crate::engine::query::QueryFilter, members: &mut Vec<String>) {
    if let Some(ref member) = filter.member {
        members.push(member.clone());
    }
    if let Some(ref and) = filter.and {
        for f in and {
            collect_filter_members(f, members);
        }
    }
    if let Some(ref or) = filter.or {
        for f in or {
            collect_filter_members(f, members);
        }
    }
}

/// Generate a WHERE clause fragment for a single filter, using quoted column names.
/// Returns None if the filter cannot be translated.
fn render_filter_sql(
    filter: &crate::engine::query::QueryFilter,
    entry: &LocalRollupEntry,
    quote: &dyn Fn(&str) -> String,
) -> Option<String> {
    use crate::engine::query::FilterOperator;

    if let (Some(ref member), Some(ref op)) = (&filter.member, &filter.operator) {
        let dim_name = member.split('.').nth(1).unwrap_or(member);
        // Resolve the column name in the rollup table
        let col = if entry.dimensions.contains(&dim_name.to_string()) {
            quote(dim_name)
        } else if entry.time_dimension.as_deref() == Some(dim_name) {
            if let Some(ref gran) = entry.granularity {
                quote(&format!("{}__{}", dim_name, gran))
            } else {
                quote(dim_name)
            }
        } else {
            return None;
        };

        let vals: Vec<String> = filter
            .values
            .iter()
            .map(|v| format!("'{}'", v.replace('\'', "''")))
            .collect();

        let sql = match op {
            FilterOperator::Equals => {
                if vals.len() == 1 {
                    format!("{} = {}", col, vals[0])
                } else {
                    format!("{} IN ({})", col, vals.join(", "))
                }
            }
            FilterOperator::NotEquals => {
                if vals.len() == 1 {
                    format!("{} <> {}", col, vals[0])
                } else {
                    format!("{} NOT IN ({})", col, vals.join(", "))
                }
            }
            FilterOperator::Gt => format!("{} > {}", col, vals.first().unwrap_or(&"NULL".into())),
            FilterOperator::Gte => {
                format!("{} >= {}", col, vals.first().unwrap_or(&"NULL".into()))
            }
            FilterOperator::Lt => format!("{} < {}", col, vals.first().unwrap_or(&"NULL".into())),
            FilterOperator::Lte => {
                format!("{} <= {}", col, vals.first().unwrap_or(&"NULL".into()))
            }
            FilterOperator::Set => format!("{} IS NOT NULL", col),
            FilterOperator::NotSet => format!("{} IS NULL", col),
            FilterOperator::Contains => format!(
                "{} LIKE '%{}%'",
                col,
                filter
                    .values
                    .first()
                    .unwrap_or(&String::new())
                    .replace('\'', "''")
            ),
            FilterOperator::NotContains => format!(
                "{} NOT LIKE '%{}%'",
                col,
                filter
                    .values
                    .first()
                    .unwrap_or(&String::new())
                    .replace('\'', "''")
            ),
            _ => return None, // date-range filters not supported in reagg
        };
        Some(sql)
    } else if let Some(ref and) = filter.and {
        let parts: Vec<String> = and
            .iter()
            .filter_map(|f| render_filter_sql(f, entry, quote))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(format!("({})", parts.join(" AND ")))
        }
    } else if let Some(ref or) = filter.or {
        let parts: Vec<String> = or
            .iter()
            .filter_map(|f| render_filter_sql(f, entry, quote))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(format!("({})", parts.join(" OR ")))
        }
    } else {
        None
    }
}

/// Build a WHERE clause from request filters for re-aggregation queries.
fn build_reagg_where_clause(
    request: &crate::engine::query::QueryRequest,
    entry: &LocalRollupEntry,
    quote: &dyn Fn(&str) -> String,
) -> String {
    let parts: Vec<String> = request
        .filters
        .iter()
        .filter_map(|f| render_filter_sql(f, entry, quote))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("\nWHERE {}", parts.join(" AND "))
    }
}

fn covers(request: &crate::engine::query::QueryRequest, entry: &LocalRollupEntry) -> bool {
    // Check that all filter dimensions exist in the rollup
    if !request.filters.is_empty() {
        let mut filter_members = Vec::new();
        for f in &request.filters {
            collect_filter_members(f, &mut filter_members);
        }
        for member in &filter_members {
            let dim_name = member.split('.').nth(1).unwrap_or(member);
            let in_dims = entry.dimensions.contains(&dim_name.to_string());
            let in_time = entry
                .time_dimension
                .as_deref()
                .is_some_and(|td| td == dim_name);
            if !in_dims && !in_time {
                return false;
            }
        }
    }

    // Extract view names from all member references
    let query_views = request.referenced_views();

    // All referenced views must match the rollup's single view
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

    // Check measures: all requested measures must be in rollup measures (and not custom).
    // Build (name, type) pairs in a single pass to avoid positional desync from filter_map.
    let rollup_measures: Vec<(&str, &str)> = entry
        .measures
        .iter()
        .filter_map(|m| {
            let name = m.get("name").and_then(|n| n.as_str())?;
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("");
            Some((name, mtype))
        })
        .collect();

    for measure in &request.measures {
        let measure_name = measure.split('.').nth(1).unwrap_or(measure);
        if let Some(&(_, mtype)) = rollup_measures.iter().find(|(n, _)| *n == measure_name) {
            // Reject types that cannot be re-aggregated
            if mtype == "custom" || mtype == "number" || mtype == "median" {
                return false;
            }
        } else {
            // Measure not found in rollup at all
            return false;
        }
    }

    // Check time dimensions
    for td in &request.time_dimensions {
        let td_name = td.dimension.split('.').nth(1).unwrap_or(&td.dimension);
        if entry.time_dimension.as_deref() != Some(td_name) {
            return false;
        }
        // Granularity: requested must be same or coarser than stored granularity
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

fn is_coarser_or_equal(requested: &str, stored: &str) -> bool {
    let order = [
        "second", "minute", "hour", "day", "week", "month", "quarter", "year",
    ];
    let req_idx = order.iter().position(|&g| g == requested);
    let stored_idx = order.iter().position(|&g| g == stored);
    match (req_idx, stored_idx) {
        (Some(r), Some(s)) => r >= s,
        _ => requested == stored,
    }
}

/// Generate a DuckDB SQL query that reads from a Parquet file and re-aggregates.
pub fn generate_reagg_sql(
    request: &crate::engine::query::QueryRequest,
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
            if let Some(ref stored_gran) = entry.granularity {
                let stored_col = format!("{}__{}", td_name, stored_gran);
                if gran == stored_gran {
                    select_cols.push(format!("\"{}\" AS \"{}\"", stored_col, alias));
                    group_by_cols.push(format!("\"{}\"", stored_col));
                } else {
                    let trunc = format!("date_trunc('{}', \"{}\")", gran, stored_col);
                    select_cols.push(format!("{} AS \"{}\"", trunc, alias));
                    group_by_cols.push(trunc);
                }
            }
        } else {
            // No requested granularity: use the stored truncated column if available,
            // otherwise fall back to the bare dimension name.
            // The rollup never stores a raw time column — only the truncated form
            // (e.g., `created_at__month`), so prefer that when present.
            let col = if let Some(ref stored_gran) = entry.granularity {
                format!("\"{}__{stored_gran}\"", td_name)
            } else {
                format!("\"{}\"", td_name)
            };
            select_cols.push(format!("{} AS \"{}\"", col, alias));
            group_by_cols.push(col);
        }
    }

    // 3. Measures (re-aggregated)
    for measure in &request.measures {
        let measure_name = measure.split('.').nth(1).unwrap_or(measure);
        let alias = measure.replace('.', "__");

        if let Some(m_meta) = entry
            .measures
            .iter()
            .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(measure_name))
        {
            let m_type = m_meta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let columns: Vec<String> = m_meta
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            match m_type {
                "sum" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__sum", measure_name));
                    select_cols.push(format!("SUM(\"{}\") AS \"{}\"", col, alias));
                }
                "count" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__count", measure_name));
                    select_cols.push(format!("SUM(\"{}\") AS \"{}\"", col, alias));
                }
                "average" => {
                    let sum_col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__sum", measure_name));
                    let count_col = columns
                        .get(1)
                        .cloned()
                        .unwrap_or_else(|| format!("{}__count", measure_name));
                    select_cols.push(format!(
                        "CAST(SUM(\"{}\") AS DOUBLE) / NULLIF(SUM(\"{}\"), 0) AS \"{}\"",
                        sum_col, count_col, alias
                    ));
                }
                "min" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__min", measure_name));
                    select_cols.push(format!("MIN(\"{}\") AS \"{}\"", col, alias));
                }
                "max" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__max", measure_name));
                    select_cols.push(format!("MAX(\"{}\") AS \"{}\"", col, alias));
                }
                "count_distinct" | "count_distinct_approx" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| measure_name.to_string());
                    select_cols.push(format!("COUNT(DISTINCT \"{}\") AS \"{}\"", col, alias));
                }
                "median" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| measure_name.to_string());
                    select_cols.push(format!("MEDIAN(\"{}\") AS \"{}\"", col, alias));
                }
                "number" => {
                    let col = columns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("{}__value", measure_name));
                    select_cols.push(format!("\"{}\" AS \"{}\"", col, alias));
                }
                _ => {
                    select_cols.push(format!("NULL AS \"{}\"", alias));
                }
            }
        }
    }

    let select = select_cols.join(", ");
    let where_clause = build_reagg_where_clause(request, entry, &|name| format!("\"{}\"", name));
    let group_by = if group_by_cols.is_empty() {
        String::new()
    } else {
        format!("\nGROUP BY {}", group_by_cols.join(", "))
    };

    let limit = request
        .limit
        .map(|l| format!("\nLIMIT {}", l))
        .unwrap_or_default();
    let offset = request
        .offset
        .map(|o| format!("\nOFFSET {}", o))
        .unwrap_or_default();

    format!(
        "SELECT {select}\nFROM read_parquet('{path}'){where_clause}{group_by}{limit}{offset}",
        path = parquet_path.replace('\'', "''"),
    )
}

/// Generate a dialect-aware SQL query that reads from a pre-aggregated warehouse table.
pub fn generate_warehouse_reagg_sql(
    request: &crate::engine::query::QueryRequest,
    entry: &LocalRollupEntry,
    table_name: &str,
    dialect: &Dialect,
) -> String {
    let mut select_cols: Vec<String> = Vec::new();
    let mut group_by_cols: Vec<String> = Vec::new();

    // 1. Dimensions
    for dim in &request.dimensions {
        let dim_name = dim.split('.').nth(1).unwrap_or(dim);
        let alias = dim.replace('.', "__");
        let col = dialect.quote_identifier(dim_name);
        let alias_q = dialect.quote_identifier(&alias);
        select_cols.push(format!("{} AS {}", col, alias_q));
        group_by_cols.push(col);
    }

    // 2. Time dimensions
    for td in &request.time_dimensions {
        let td_name = td.dimension.split('.').nth(1).unwrap_or(&td.dimension);
        let alias = td.dimension.replace('.', "__");
        let alias_q = dialect.quote_identifier(&alias);
        if let Some(ref gran) = td.granularity {
            if let Some(ref stored_gran) = entry.granularity {
                let stored_col_name = format!("{}__{}", td_name, stored_gran);
                let stored_col = dialect.quote_identifier(&stored_col_name);
                if gran == stored_gran {
                    select_cols.push(format!("{} AS {}", stored_col, alias_q));
                    group_by_cols.push(stored_col);
                } else {
                    let trunc = dialect.date_trunc(gran, &stored_col);
                    select_cols.push(format!("{} AS {}", trunc, alias_q));
                    group_by_cols.push(trunc);
                }
            }
        } else {
            let col = if let Some(ref stored_gran) = entry.granularity {
                dialect.quote_identifier(&format!("{}__{}", td_name, stored_gran))
            } else {
                dialect.quote_identifier(td_name)
            };
            select_cols.push(format!("{} AS {}", col, alias_q));
            group_by_cols.push(col);
        }
    }

    // 3. Measures (re-aggregated)
    for measure in &request.measures {
        let measure_name = measure.split('.').nth(1).unwrap_or(measure);
        let alias = measure.replace('.', "__");
        let alias_q = dialect.quote_identifier(&alias);

        if let Some(m_meta) = entry
            .measures
            .iter()
            .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(measure_name))
        {
            let m_type = m_meta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let columns: Vec<String> = m_meta
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            match m_type {
                "sum" => {
                    let col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{}__sum", measure_name)),
                    );
                    select_cols.push(format!("SUM({}) AS {}", col, alias_q));
                }
                "count" => {
                    let col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{}__count", measure_name)),
                    );
                    select_cols.push(format!("SUM({}) AS {}", col, alias_q));
                }
                "average" => {
                    let sum_col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{}__sum", measure_name)),
                    );
                    let count_col = dialect.quote_identifier(
                        &columns
                            .get(1)
                            .cloned()
                            .unwrap_or_else(|| format!("{}__count", measure_name)),
                    );
                    let sum_expr = format!("SUM({})", sum_col);
                    let count_expr = format!("NULLIF(SUM({}), 0)", count_col);
                    select_cols.push(format!(
                        "{} / {} AS {}",
                        dialect.cast_to_double(&sum_expr),
                        count_expr,
                        alias_q,
                    ));
                }
                "min" => {
                    let col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{}__min", measure_name)),
                    );
                    select_cols.push(format!("MIN({}) AS {}", col, alias_q));
                }
                "max" => {
                    let col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{}__max", measure_name)),
                    );
                    select_cols.push(format!("MAX({}) AS {}", col, alias_q));
                }
                "count_distinct" | "count_distinct_approx" => {
                    let col = dialect.quote_identifier(
                        &columns
                            .first()
                            .cloned()
                            .unwrap_or_else(|| measure_name.to_string()),
                    );
                    select_cols.push(format!("COUNT(DISTINCT {}) AS {}", col, alias_q));
                }
                _ => {
                    select_cols.push(format!("NULL AS {}", alias_q));
                }
            }
        }
    }

    let select = select_cols.join(", ");
    let dialect_clone = dialect.clone();
    let where_clause =
        build_reagg_where_clause(request, entry, &|name| dialect_clone.quote_identifier(name));
    let group_by = if group_by_cols.is_empty() {
        String::new()
    } else {
        format!(
            "\nGROUP BY {}",
            group_by_cols
                .iter()
                .enumerate()
                .map(|(i, _)| format!("{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let limit = request
        .limit
        .map(|l| format!("\nLIMIT {}", l))
        .unwrap_or_default();
    let offset = request
        .offset
        .map(|o| format!("\nOFFSET {}", o))
        .unwrap_or_default();

    format!("SELECT {select}\nFROM {table_name}{where_clause}{group_by}{limit}{offset}",)
}

/// Build a ManifestEntry from a view and rollup spec.
pub fn build_manifest_entry(
    view: &View,
    rollup: &RollupSpec,
    schema: &str,
    date_str: &str,
) -> ManifestEntry {
    let table_name = format!("{}__{}__{}", view.name, rollup.hash, date_str);

    let measures_json = serde_json::to_string(
        &rollup
            .measures
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "type": m.measure_type.to_string(),
                    "columns": m.columns,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    ManifestEntry {
        view_name: view.name.clone(),
        rollup_name: rollup.name.clone(),
        rollup_hash: rollup.hash.clone(),
        table_name: format!("{}.{}", schema, table_name),
        dimensions: rollup.dimensions.clone(),
        measures_json,
        time_dimension: rollup.time_dimension.clone(),
        granularity: rollup.granularity.clone(),
        // Convert YYYYMMDD to YYYY-MM-DD for SQL DATE columns
        build_date: if date_str.len() == 8 && date_str.chars().all(|c| c.is_ascii_digit()) {
            format!("{}-{}-{}", &date_str[..4], &date_str[4..6], &date_str[6..8])
        } else {
            date_str.to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::query::QueryRequest;

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
        assert_eq!(sqls.len(), 2); // DROP + CTAS
        let ctas = &sqls[1];
        assert!(
            ctas.contains("CREATE TABLE"),
            "Missing CREATE TABLE: {}",
            ctas
        );
        assert!(ctas.contains("AIRLAYER"), "Missing schema: {}", ctas);
        assert!(ctas.contains("orders__"), "Missing view name: {}", ctas);
        assert!(ctas.contains("20260415"), "Missing date: {}", ctas);
        assert!(ctas.contains("SUM("), "Missing SUM aggregation: {}", ctas);
        assert!(
            ctas.contains("total_revenue__sum"),
            "Missing column alias: {}",
            ctas
        );
        assert!(
            ctas.contains("toStartOfMonth"),
            "Missing ClickHouse date_trunc: {}",
            ctas
        );
    }

    #[test]
    fn test_generate_manifest_sql_clickhouse() {
        let create = generate_manifest_create_sql("AIRLAYER", &crate::dialect::Dialect::ClickHouse);
        assert!(
            create.contains("__manifest"),
            "Missing manifest: {}",
            create
        );
        assert!(
            create.contains("ReplacingMergeTree"),
            "Missing engine: {}",
            create
        );
    }

    #[test]
    fn test_generate_manifest_sql_postgres() {
        let create = generate_manifest_create_sql("preagg", &crate::dialect::Dialect::Postgres);
        assert!(
            create.contains("\"preagg\".\"__manifest\""),
            "Missing quoted name: {}",
            create
        );
        assert!(create.contains("PRIMARY KEY"), "Missing PK: {}", create);
    }

    #[test]
    fn test_generate_manifest_sql_bigquery() {
        let create = generate_manifest_create_sql("my_dataset", &crate::dialect::Dialect::BigQuery);
        assert!(
            create.contains("`my_dataset`.`__manifest`"),
            "Missing backtick-quoted name: {}",
            create
        );
        assert!(create.contains("STRING"), "Missing STRING type: {}", create);
        assert!(
            !create.contains("PRIMARY KEY"),
            "BigQuery should not have PK: {}",
            create
        );
    }

    #[test]
    fn test_generate_manifest_sql_sqlite() {
        let create = generate_manifest_create_sql("preagg", &crate::dialect::Dialect::SQLite);
        assert!(create.contains("TEXT"), "Missing TEXT type: {}", create);
        assert!(create.contains("UNIQUE"), "Missing UNIQUE: {}", create);
        assert!(
            !create.contains("PRIMARY KEY"),
            "SQLite should use UNIQUE not PK: {}",
            create
        );
    }

    #[test]
    fn test_build_sql_uses_dialect_quoting() {
        let view = test_view_with_preaggs();
        let rollups = resolve_rollups(&view);
        // BigQuery should use backtick quoting
        let sqls = generate_build_sql(
            &view,
            &rollups[0],
            "my_dataset",
            "20260415",
            &crate::dialect::Dialect::BigQuery,
        );
        let ctas = &sqls[1];
        assert!(
            ctas.contains("`my_dataset`"),
            "Missing backtick-quoted schema: {}",
            ctas
        );
    }

    #[test]
    fn test_manifest_upsert_sqlite_uses_replace() {
        let entry = ManifestEntry {
            view_name: "orders".into(),
            rollup_name: "by_region".into(),
            rollup_hash: "a1b2c3d4".into(),
            table_name: "preagg.orders__a1b2c3d4__20260415".into(),
            dimensions: vec!["region".into()],
            measures_json: "[]".into(),
            time_dimension: None,
            granularity: None,
            build_date: "2026-04-15".into(),
        };
        let stmts =
            generate_manifest_upsert_sql("preagg", &entry, &crate::dialect::Dialect::SQLite);
        assert_eq!(stmts.len(), 1, "SQLite should use INSERT OR REPLACE");
        assert!(
            stmts[0].contains("INSERT OR REPLACE"),
            "Missing INSERT OR REPLACE: {}",
            stmts[0]
        );
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
        let stmts =
            generate_manifest_upsert_sql("AIRLAYER", &entry, &crate::dialect::Dialect::ClickHouse);
        assert_eq!(stmts.len(), 1, "ClickHouse should produce only INSERT");
        assert!(
            stmts[0].contains("INSERT INTO"),
            "Missing INSERT: {}",
            stmts[0]
        );
        assert!(
            stmts[0].contains("orders"),
            "Missing view name: {}",
            stmts[0]
        );

        // Non-ClickHouse should produce DELETE + INSERT
        let stmts_duckdb =
            generate_manifest_upsert_sql("AIRLAYER", &entry, &crate::dialect::Dialect::DuckDB);
        assert_eq!(
            stmts_duckdb.len(),
            2,
            "DuckDB should produce DELETE + INSERT"
        );
        assert!(
            stmts_duckdb[0].contains("DELETE FROM"),
            "Missing DELETE: {}",
            stmts_duckdb[0]
        );
        assert!(
            stmts_duckdb[1].contains("INSERT INTO"),
            "Missing INSERT: {}",
            stmts_duckdb[1]
        );
    }

    #[test]
    fn test_rollup_hash_deterministic() {
        let h1 = compute_rollup_hash(
            &["region".into(), "status".into()],
            &["revenue".into()],
            Some("created_at"),
            Some("month"),
        );
        let h2 = compute_rollup_hash(
            &["region".into(), "status".into()],
            &["revenue".into()],
            Some("created_at"),
            Some("month"),
        );
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_rollup_hash_order_independent() {
        let h1 = compute_rollup_hash(
            &["region".into(), "status".into()],
            &["a".into(), "b".into()],
            None,
            None,
        );
        let h2 = compute_rollup_hash(
            &["status".into(), "region".into()],
            &["b".into(), "a".into()],
            None,
            None,
        );
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_rollup_hash_different_inputs() {
        let h1 = compute_rollup_hash(
            &["region".into()],
            &["revenue".into()],
            Some("created_at"),
            Some("month"),
        );
        let h2 = compute_rollup_hash(
            &["status".into()],
            &["revenue".into()],
            Some("created_at"),
            Some("month"),
        );
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
        assert!(rollups[0]
            .measures
            .iter()
            .any(|m| m.name == "total_revenue"));
    }

    fn test_view_with_preaggs() -> View {
        use crate::schema::models::*;
        View {
            name: "orders".to_string(),
            description: Some("test".to_string()),
            label: None,
            datasource: None,
            dialect: None,
            table: Some("orders".to_string()),
            sql: None,
            entities: vec![],
            dimensions: vec![
                Dimension {
                    name: "region".into(),
                    dimension_type: DimensionType::String,
                    description: None,
                    expr: "region".into(),
                    original_expr: None,
                    samples: None,
                    synonyms: None,
                    primary_key: None,
                    sub_query: None,
                    inherits_from: None,
                    meta: None,
                },
                Dimension {
                    name: "created_at".into(),
                    dimension_type: DimensionType::Datetime,
                    description: None,
                    expr: "created_at".into(),
                    original_expr: None,
                    samples: None,
                    synonyms: None,
                    primary_key: None,
                    sub_query: None,
                    inherits_from: None,
                    meta: None,
                },
            ],
            measures: Some(vec![Measure {
                name: "total_revenue".into(),
                measure_type: MeasureType::Sum,
                description: None,
                expr: Some("revenue".into()),
                original_expr: None,
                filters: None,
                samples: None,
                synonyms: None,
                rolling_window: None,
                inherits_from: None,
                meta: None,
            }]),
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
            description: Some("test".into()),
            label: None,
            datasource: None,
            dialect: None,
            table: Some("orders".into()),
            sql: None,
            entities: vec![],
            dimensions: vec![
                Dimension {
                    name: "region".into(),
                    dimension_type: DimensionType::String,
                    description: None,
                    expr: "region".into(),
                    original_expr: None,
                    samples: None,
                    synonyms: None,
                    primary_key: None,
                    sub_query: None,
                    inherits_from: None,
                    meta: None,
                },
                Dimension {
                    name: "created_at".into(),
                    dimension_type: DimensionType::Datetime,
                    description: None,
                    expr: "created_at".into(),
                    original_expr: None,
                    samples: None,
                    synonyms: None,
                    primary_key: None,
                    sub_query: None,
                    inherits_from: None,
                    meta: None,
                },
            ],
            measures: Some(vec![
                Measure {
                    name: "total_revenue".into(),
                    measure_type: MeasureType::Sum,
                    description: None,
                    expr: Some("revenue".into()),
                    original_expr: None,
                    filters: None,
                    samples: None,
                    synonyms: None,
                    rolling_window: None,
                    inherits_from: None,
                    meta: None,
                },
                Measure {
                    name: "avg_revenue".into(),
                    measure_type: MeasureType::Average,
                    description: None,
                    expr: Some("revenue".into()),
                    original_expr: None,
                    filters: None,
                    samples: None,
                    synonyms: None,
                    rolling_window: None,
                    inherits_from: None,
                    meta: None,
                },
            ]),
            segments: vec![],
            pre_aggregations: None,
            meta: None,
        }
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

    #[test]
    fn test_reagg_sql_basic() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        assert!(
            sql.contains("read_parquet('/data/orders.parquet')"),
            "Missing FROM: {}",
            sql
        );
        assert!(
            sql.contains("SUM(\"total_revenue__sum\")"),
            "Missing SUM re-agg: {}",
            sql
        );
        assert!(
            sql.contains("\"region\""),
            "Missing dimension column: {}",
            sql
        );
        assert!(sql.contains("GROUP BY"), "Missing GROUP BY: {}", sql);
    }

    #[test]
    fn test_reagg_sql_with_time_dimension_same_gran() {
        use crate::engine::query::TimeDimensionQuery;
        let entry = test_local_rollup_entry(); // stored gran = "month"
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            time_dimensions: vec![TimeDimensionQuery {
                dimension: "orders.created_at".to_string(),
                granularity: Some("month".to_string()),
                date_range: None,
            }],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        // Same granularity: should select the stored column directly, no date_trunc
        assert!(
            sql.contains("\"created_at__month\""),
            "Missing stored time col: {}",
            sql
        );
        assert!(
            !sql.contains("date_trunc"),
            "Should not re-truncate same gran: {}",
            sql
        );
    }

    #[test]
    fn test_reagg_sql_with_time_dimension_coarser_gran() {
        use crate::engine::query::TimeDimensionQuery;
        let entry = test_local_rollup_entry(); // stored gran = "month"
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            time_dimensions: vec![TimeDimensionQuery {
                dimension: "orders.created_at".to_string(),
                granularity: Some("year".to_string()),
                date_range: None,
            }],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        // Coarser granularity: should apply date_trunc over the stored monthly column
        assert!(
            sql.contains("date_trunc('year', \"created_at__month\")"),
            "Missing date_trunc: {}",
            sql
        );
    }

    #[test]
    fn test_reagg_sql_no_gran_uses_stored_col() {
        use crate::engine::query::TimeDimensionQuery;
        let entry = test_local_rollup_entry(); // stored gran = "month"
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            time_dimensions: vec![TimeDimensionQuery {
                dimension: "orders.created_at".to_string(),
                granularity: None,
                date_range: None,
            }],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        // No requested gran: should fall back to the stored truncated column, not bare "created_at"
        assert!(
            sql.contains("\"created_at__month\""),
            "Should use stored truncated col: {}",
            sql
        );
        assert!(
            !sql.contains("\"created_at\""),
            "Should not select bare column: {}",
            sql
        );
    }

    #[test]
    fn test_reagg_sql_parquet_path_escaping() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/it's_here.parquet");
        assert!(
            sql.contains("it''s_here"),
            "Single quote should be escaped: {}",
            sql
        );
    }

    #[test]
    fn test_reagg_sql_limit_offset() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            limit: Some(100),
            offset: Some(20),
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        assert!(sql.contains("LIMIT 100"), "Missing LIMIT: {}", sql);
        assert!(sql.contains("OFFSET 20"), "Missing OFFSET: {}", sql);
    }

    #[test]
    fn test_warehouse_reagg_sql_substitutes_table() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        let sql = generate_warehouse_reagg_sql(
            &request,
            &entry,
            "AIRLAYER.orders__a1b2c3d4__20260415",
            &crate::dialect::Dialect::ClickHouse,
        );
        assert!(
            !sql.contains("read_parquet"),
            "Should not have read_parquet: {}",
            sql
        );
        assert!(
            sql.contains("AIRLAYER.orders__a1b2c3d4__20260415"),
            "Missing table name: {}",
            sql
        );
    }

    #[test]
    fn test_coverage_check_covered() {
        let entry = test_local_rollup_entry();
        let rollups = [entry];
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        let result = check_coverage(&request, &rollups);
        assert!(result.is_some(), "Expected coverage match");
    }

    #[test]
    fn test_coverage_check_not_covered_missing_dim() {
        let entry = test_local_rollup_entry();
        let rollups = [entry];
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.status".to_string()], // Not in rollup
            ..QueryRequest::new()
        };
        let result = check_coverage(&request, &rollups);
        assert!(result.is_none(), "Expected no coverage match");
    }

    #[test]
    fn test_coverage_check_not_covered_missing_measure() {
        let entry = test_local_rollup_entry();
        let rollups = [entry];
        let request = QueryRequest {
            measures: vec!["orders.other_metric".to_string()], // Not in rollup
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        let result = check_coverage(&request, &rollups);
        assert!(result.is_none(), "Expected no coverage match");
    }

    #[test]
    fn test_coverage_rejects_median_and_number_measures() {
        let entry = LocalRollupEntry {
            view_name: "orders".into(),
            rollup_name: "test".into(),
            rollup_hash: "abc".into(),
            file: "test.parquet".into(),
            dimensions: vec!["region".into()],
            measures: vec![
                serde_json::json!({"name": "med_rev", "type": "median", "columns": ["revenue", "revenue__freq"]}),
                serde_json::json!({"name": "computed", "type": "number", "columns": ["computed__value"]}),
            ],
            time_dimension: None,
            granularity: None,
            build_date: "2026-04-16".into(),
        };
        let rollups = [entry];

        let request = QueryRequest {
            measures: vec!["orders.med_rev".to_string()],
            ..QueryRequest::new()
        };
        assert!(
            check_coverage(&request, &rollups).is_none(),
            "Median should not be covered"
        );

        let request = QueryRequest {
            measures: vec!["orders.computed".to_string()],
            ..QueryRequest::new()
        };
        assert!(
            check_coverage(&request, &rollups).is_none(),
            "Number should not be covered"
        );
    }

    #[test]
    fn test_coverage_allows_filtered_query_when_dim_in_rollup() {
        use crate::engine::query::{FilterOperator, QueryFilter};
        let entry = test_local_rollup_entry();
        let rollups = [entry];
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            filters: vec![QueryFilter {
                member: Some("orders.region".to_string()),
                operator: Some(FilterOperator::Equals),
                values: vec!["US".to_string()],
                and: None,
                or: None,
            }],
            ..QueryRequest::new()
        };
        let result = check_coverage(&request, &rollups);
        assert!(
            result.is_some(),
            "Filter on rollup dimension should be covered"
        );
    }

    #[test]
    fn test_coverage_rejects_filter_on_missing_dim() {
        use crate::engine::query::{FilterOperator, QueryFilter};
        let entry = test_local_rollup_entry();
        let rollups = [entry];
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            filters: vec![QueryFilter {
                member: Some("orders.status".to_string()), // Not in rollup
                operator: Some(FilterOperator::Equals),
                values: vec!["active".to_string()],
                and: None,
                or: None,
            }],
            ..QueryRequest::new()
        };
        let result = check_coverage(&request, &rollups);
        assert!(
            result.is_none(),
            "Filter on non-rollup dimension should not be covered"
        );
    }

    #[test]
    fn test_reagg_sql_with_filter() {
        use crate::engine::query::{FilterOperator, QueryFilter};
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            filters: vec![QueryFilter {
                member: Some("orders.region".to_string()),
                operator: Some(FilterOperator::Equals),
                values: vec!["US".to_string()],
                and: None,
                or: None,
            }],
            ..QueryRequest::new()
        };
        let sql = generate_reagg_sql(&request, &entry, "/data/orders.parquet");
        assert!(
            sql.contains("WHERE \"region\" = 'US'"),
            "Missing WHERE clause: {}",
            sql
        );
    }

    #[test]
    fn test_warehouse_reagg_sql_dialect_aware() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        // Postgres should use double-quote identifiers
        let sql = generate_warehouse_reagg_sql(
            &request,
            &entry,
            "\"preagg\".\"orders__abc__20260415\"",
            &crate::dialect::Dialect::Postgres,
        );
        assert!(
            sql.contains("\"region\""),
            "Should use double-quote identifiers: {}",
            sql
        );
        assert!(
            sql.contains("SUM(\"total_revenue__sum\")"),
            "Should have SUM re-agg: {}",
            sql
        );

        // BigQuery should use backtick identifiers
        let sql = generate_warehouse_reagg_sql(
            &request,
            &entry,
            "`my_dataset`.`orders__abc__20260415`",
            &crate::dialect::Dialect::BigQuery,
        );
        assert!(
            sql.contains("`region`"),
            "Should use backtick identifiers: {}",
            sql
        );
    }

    #[test]
    fn test_warehouse_reagg_sql_average_uses_cast() {
        let entry = LocalRollupEntry {
            view_name: "orders".into(),
            rollup_name: "test".into(),
            rollup_hash: "abc".into(),
            file: "test.parquet".into(),
            dimensions: vec![],
            measures: vec![serde_json::json!({
                "name": "avg_rev", "type": "average",
                "columns": ["avg_rev__sum", "avg_rev__count"]
            })],
            time_dimension: None,
            granularity: None,
            build_date: "2026-04-16".into(),
        };
        let request = QueryRequest {
            measures: vec!["orders.avg_rev".to_string()],
            ..QueryRequest::new()
        };
        let sql = generate_warehouse_reagg_sql(
            &request,
            &entry,
            "preagg.test",
            &crate::dialect::Dialect::Postgres,
        );
        assert!(
            sql.contains("CAST(SUM(\"avg_rev__sum\") AS DOUBLE PRECISION)"),
            "Postgres should use DOUBLE PRECISION: {}",
            sql
        );

        let sql = generate_warehouse_reagg_sql(
            &request,
            &entry,
            "preagg.test",
            &crate::dialect::Dialect::BigQuery,
        );
        assert!(
            sql.contains("CAST(SUM(`avg_rev__sum`) AS FLOAT64)"),
            "BigQuery should use FLOAT64: {}",
            sql
        );
    }

    #[test]
    fn test_default_rollup_excludes_median_and_number() {
        use crate::schema::models::*;
        let view = View {
            name: "test".into(),
            description: Some("test".into()),
            label: None,
            datasource: None,
            dialect: None,
            table: Some("test".into()),
            sql: None,
            entities: vec![],
            dimensions: vec![Dimension {
                name: "region".into(),
                dimension_type: DimensionType::String,
                description: None,
                expr: "region".into(),
                original_expr: None,
                samples: None,
                synonyms: None,
                primary_key: None,
                sub_query: None,
                inherits_from: None,
                meta: None,
            }],
            measures: Some(vec![
                Measure {
                    name: "total".into(),
                    measure_type: MeasureType::Sum,
                    description: None,
                    expr: Some("amount".into()),
                    original_expr: None,
                    filters: None,
                    samples: None,
                    synonyms: None,
                    rolling_window: None,
                    inherits_from: None,
                    meta: None,
                },
                Measure {
                    name: "med".into(),
                    measure_type: MeasureType::Median,
                    description: None,
                    expr: Some("amount".into()),
                    original_expr: None,
                    filters: None,
                    samples: None,
                    synonyms: None,
                    rolling_window: None,
                    inherits_from: None,
                    meta: None,
                },
                Measure {
                    name: "computed".into(),
                    measure_type: MeasureType::Number,
                    description: None,
                    expr: Some("amount / qty".into()),
                    original_expr: None,
                    filters: None,
                    samples: None,
                    synonyms: None,
                    rolling_window: None,
                    inherits_from: None,
                    meta: None,
                },
            ]),
            segments: vec![],
            pre_aggregations: None,
            meta: None,
        };
        let rollups = resolve_rollups(&view);
        assert_eq!(rollups.len(), 1);
        let measure_names: Vec<&str> = rollups[0]
            .measures
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(measure_names.contains(&"total"), "Sum should be included");
        assert!(!measure_names.contains(&"med"), "Median should be excluded");
        assert!(
            !measure_names.contains(&"computed"),
            "Number should be excluded"
        );
    }

    // ── Comprehensive all-dialects tests ────────────────────────────────────

    fn all_dialects() -> Vec<Dialect> {
        vec![
            Dialect::Postgres,
            Dialect::MySQL,
            Dialect::BigQuery,
            Dialect::Snowflake,
            Dialect::DuckDB,
            Dialect::ClickHouse,
            Dialect::Databricks,
            Dialect::Redshift,
            Dialect::SQLite,
            Dialect::Domo,
            Dialect::Presto,
        ]
    }

    /// Helper: build a rollup entry with sum + average + count_distinct measures.
    fn rich_local_rollup_entry() -> LocalRollupEntry {
        LocalRollupEntry {
            view_name: "orders".into(),
            rollup_name: "by_region_monthly".into(),
            rollup_hash: "a1b2c3d4".into(),
            file: "orders__a1b2c3d4.parquet".into(),
            dimensions: vec!["region".into(), "status".into()],
            measures: vec![
                serde_json::json!({"name": "total_revenue", "type": "sum", "columns": ["total_revenue__sum"]}),
                serde_json::json!({"name": "avg_price", "type": "average", "columns": ["avg_price__sum", "avg_price__count"]}),
                serde_json::json!({"name": "event_count", "type": "count", "columns": ["event_count__count"]}),
                serde_json::json!({"name": "max_amount", "type": "max", "columns": ["max_amount__max"]}),
                serde_json::json!({"name": "min_amount", "type": "min", "columns": ["min_amount__min"]}),
                serde_json::json!({"name": "unique_users", "type": "count_distinct", "columns": ["user_id"]}),
            ],
            time_dimension: Some("created_at".into()),
            granularity: Some("month".into()),
            build_date: "2026-04-16".into(),
        }
    }

    #[test]
    fn test_build_sql_all_dialects() {
        let view = test_view_with_preaggs();
        let rollups = resolve_rollups(&view);
        for dialect in all_dialects() {
            let sqls = generate_build_sql(&view, &rollups[0], "preagg", "20260416", &dialect);
            assert_eq!(sqls.len(), 2, "{}: expected DROP + CTAS", dialect);
            let drop = &sqls[0];
            let ctas = &sqls[1];
            assert!(
                drop.contains("DROP TABLE IF EXISTS"),
                "{}: missing DROP: {}",
                dialect,
                drop
            );
            assert!(
                ctas.contains("CREATE TABLE"),
                "{}: missing CREATE TABLE: {}",
                ctas,
                dialect
            );
            assert!(ctas.contains("SUM("), "{}: missing SUM: {}", dialect, ctas);
            assert!(
                ctas.contains("GROUP BY"),
                "{}: missing GROUP BY: {}",
                dialect,
                ctas
            );
            // ClickHouse should have MergeTree
            if dialect == Dialect::ClickHouse {
                assert!(
                    ctas.contains("MergeTree"),
                    "ClickHouse CTAS should have MergeTree: {}",
                    ctas
                );
            }
            // BigQuery/MySQL/Databricks/Domo should use backtick quoting
            if matches!(
                dialect,
                Dialect::BigQuery | Dialect::MySQL | Dialect::Databricks | Dialect::Domo
            ) {
                assert!(
                    ctas.contains('`'),
                    "{}: should use backtick quoting: {}",
                    dialect,
                    ctas
                );
            }
            // Snowflake should uppercase
            if dialect == Dialect::Snowflake {
                assert!(
                    ctas.contains("\"PREAGG\""),
                    "Snowflake should uppercase schema: {}",
                    ctas
                );
            }
        }
    }

    #[test]
    fn test_manifest_create_sql_all_dialects() {
        for dialect in all_dialects() {
            let sql = generate_manifest_create_sql("preagg", &dialect);
            assert!(
                sql.contains("CREATE TABLE IF NOT EXISTS"),
                "{}: missing CREATE: {}",
                dialect,
                sql
            );
            let sql_lower = sql.to_lowercase();
            assert!(
                sql_lower.contains("__manifest"),
                "{}: missing manifest: {}",
                dialect,
                sql
            );
            // Check type names
            match dialect {
                Dialect::ClickHouse => {
                    assert!(
                        sql.contains("String"),
                        "{}: missing String type: {}",
                        dialect,
                        sql
                    );
                    assert!(
                        sql.contains("ReplacingMergeTree"),
                        "{}: missing engine: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::BigQuery => {
                    assert!(
                        sql.contains("STRING"),
                        "{}: missing STRING type: {}",
                        dialect,
                        sql
                    );
                    assert!(
                        !sql.contains("PRIMARY KEY"),
                        "{}: BigQuery should not have PK: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::SQLite => {
                    assert!(
                        sql.contains("TEXT"),
                        "{}: missing TEXT type: {}",
                        dialect,
                        sql
                    );
                    assert!(
                        sql.contains("UNIQUE"),
                        "{}: missing UNIQUE: {}",
                        dialect,
                        sql
                    );
                }
                _ => {
                    assert!(
                        sql.contains("VARCHAR"),
                        "{}: missing VARCHAR type: {}",
                        dialect,
                        sql
                    );
                    assert!(
                        sql.contains("PRIMARY KEY"),
                        "{}: missing PK: {}",
                        dialect,
                        sql
                    );
                }
            }
        }
    }

    #[test]
    fn test_manifest_upsert_all_dialects() {
        let entry = ManifestEntry {
            view_name: "orders".into(),
            rollup_name: "by_region".into(),
            rollup_hash: "a1b2c3d4".into(),
            table_name: "preagg.orders__a1b2c3d4__20260416".into(),
            dimensions: vec!["region".into()],
            measures_json: "[]".into(),
            time_dimension: None,
            granularity: None,
            build_date: "2026-04-16".into(),
        };
        for dialect in all_dialects() {
            let stmts = generate_manifest_upsert_sql("preagg", &entry, &dialect);
            match dialect {
                Dialect::ClickHouse => {
                    assert_eq!(stmts.len(), 1, "{}: ClickHouse should have 1 stmt", dialect);
                    assert!(
                        stmts[0].starts_with("INSERT INTO"),
                        "{}: should be INSERT: {}",
                        dialect,
                        stmts[0]
                    );
                }
                Dialect::SQLite => {
                    assert_eq!(stmts.len(), 1, "{}: SQLite should have 1 stmt", dialect);
                    assert!(
                        stmts[0].contains("INSERT OR REPLACE"),
                        "{}: should be INSERT OR REPLACE: {}",
                        dialect,
                        stmts[0]
                    );
                }
                _ => {
                    assert_eq!(stmts.len(), 2, "{}: should have DELETE + INSERT", dialect);
                    assert!(
                        stmts[0].contains("DELETE FROM"),
                        "{}: first should be DELETE: {}",
                        dialect,
                        stmts[0]
                    );
                    assert!(
                        stmts[1].starts_with("INSERT INTO"),
                        "{}: second should be INSERT: {}",
                        dialect,
                        stmts[1]
                    );
                }
            }
        }
    }

    #[test]
    fn test_warehouse_reagg_sql_all_dialects_basic() {
        let entry = test_local_rollup_entry(); // sum measure, region dim, month time dim
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        for dialect in all_dialects() {
            let table = dialect.qualify_table("preagg", "orders__a1b2c3d4__20260416");
            let sql = generate_warehouse_reagg_sql(&request, &entry, &table, &dialect);

            // All dialects should have SELECT, FROM, GROUP BY
            assert!(
                sql.contains("SELECT"),
                "{}: missing SELECT: {}",
                dialect,
                sql
            );
            assert!(
                sql.contains(&table),
                "{}: missing table name: {}",
                dialect,
                sql
            );
            assert!(
                sql.contains("GROUP BY"),
                "{}: missing GROUP BY: {}",
                dialect,
                sql
            );
            assert!(sql.contains("SUM("), "{}: missing SUM: {}", dialect, sql);
        }
    }

    #[test]
    fn test_warehouse_reagg_sql_all_dialects_time_coarser_gran() {
        use crate::engine::query::TimeDimensionQuery;
        let entry = test_local_rollup_entry(); // stored gran = "month"
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            time_dimensions: vec![TimeDimensionQuery {
                dimension: "orders.created_at".to_string(),
                granularity: Some("year".to_string()),
                date_range: None,
            }],
            ..QueryRequest::new()
        };
        for dialect in all_dialects() {
            let table = dialect.qualify_table("preagg", "orders__a1b2c3d4__20260416");
            let sql = generate_warehouse_reagg_sql(&request, &entry, &table, &dialect);

            // Should contain the dialect-specific date truncation
            match dialect {
                Dialect::MySQL | Dialect::Domo => {
                    // MySQL uses DATE_FORMAT for year truncation
                    assert!(
                        sql.contains("DATE_FORMAT("),
                        "{}: should use DATE_FORMAT for year: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::BigQuery => {
                    assert!(
                        sql.contains("TIMESTAMP_TRUNC("),
                        "{}: should use TIMESTAMP_TRUNC: {}",
                        dialect,
                        sql
                    );
                    assert!(
                        sql.contains("YEAR"),
                        "{}: should have YEAR granularity: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::ClickHouse => {
                    assert!(
                        sql.contains("toStartOfYear("),
                        "{}: should use toStartOfYear: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::Snowflake | Dialect::Presto => {
                    assert!(
                        sql.contains("DATE_TRUNC('year'"),
                        "{}: should use DATE_TRUNC: {}",
                        dialect,
                        sql
                    );
                }
                _ => {
                    // Postgres, DuckDB, Redshift, SQLite, Databricks — lowercase date_trunc
                    assert!(
                        sql.contains("date_trunc('year'"),
                        "{}: should use date_trunc: {}",
                        dialect,
                        sql
                    );
                }
            }
        }
    }

    #[test]
    fn test_warehouse_reagg_sql_all_dialects_average() {
        let entry = rich_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.avg_price".to_string()],
            ..QueryRequest::new()
        };
        for dialect in all_dialects() {
            let table = dialect.qualify_table("preagg", "test");
            let sql = generate_warehouse_reagg_sql(&request, &entry, &table, &dialect);
            // All should use CAST + SUM/NULLIF pattern
            assert!(sql.contains("CAST("), "{}: missing CAST: {}", dialect, sql);
            assert!(
                sql.contains("NULLIF("),
                "{}: missing NULLIF: {}",
                dialect,
                sql
            );
            // Check dialect-specific cast type
            match dialect {
                Dialect::Postgres | Dialect::Redshift => {
                    assert!(
                        sql.contains("DOUBLE PRECISION"),
                        "{}: should use DOUBLE PRECISION: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::BigQuery => {
                    assert!(
                        sql.contains("FLOAT64"),
                        "{}: should use FLOAT64: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::ClickHouse => {
                    assert!(
                        sql.contains("Float64"),
                        "{}: should use Float64: {}",
                        dialect,
                        sql
                    );
                }
                Dialect::MySQL | Dialect::Domo => {
                    assert!(
                        sql.contains("DECIMAL(38,10)"),
                        "{}: should use DECIMAL: {}",
                        dialect,
                        sql
                    );
                }
                _ => {
                    assert!(
                        sql.contains("AS DOUBLE)"),
                        "{}: should use DOUBLE: {}",
                        dialect,
                        sql
                    );
                }
            }
        }
    }

    #[test]
    fn test_warehouse_reagg_sql_all_dialects_all_measure_types() {
        let entry = rich_local_rollup_entry();
        // Request all supported measure types
        let request = QueryRequest {
            measures: vec![
                "orders.total_revenue".to_string(),
                "orders.event_count".to_string(),
                "orders.avg_price".to_string(),
                "orders.max_amount".to_string(),
                "orders.min_amount".to_string(),
                "orders.unique_users".to_string(),
            ],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        for dialect in all_dialects() {
            let table = dialect.qualify_table("preagg", "test");
            let sql = generate_warehouse_reagg_sql(&request, &entry, &table, &dialect);

            assert!(
                sql.contains("SUM("),
                "{}: missing SUM for sum/count: {}",
                dialect,
                sql
            );
            assert!(sql.contains("MAX("), "{}: missing MAX: {}", dialect, sql);
            assert!(sql.contains("MIN("), "{}: missing MIN: {}", dialect, sql);
            assert!(
                sql.contains("COUNT(DISTINCT"),
                "{}: missing COUNT DISTINCT: {}",
                dialect,
                sql
            );
            assert!(
                sql.contains("CAST("),
                "{}: missing CAST for avg: {}",
                dialect,
                sql
            );
        }
    }

    #[test]
    fn test_warehouse_reagg_sql_snowflake_uppercase() {
        let entry = test_local_rollup_entry();
        let request = QueryRequest {
            measures: vec!["orders.total_revenue".to_string()],
            dimensions: vec!["orders.region".to_string()],
            ..QueryRequest::new()
        };
        let table = Dialect::Snowflake.qualify_table("preagg", "orders__abc__20260416");
        let sql = generate_warehouse_reagg_sql(&request, &entry, &table, &Dialect::Snowflake);

        // Snowflake uppercases all identifiers
        assert!(
            sql.contains("\"REGION\""),
            "Snowflake should uppercase dimension: {}",
            sql
        );
        assert!(
            sql.contains("\"TOTAL_REVENUE__SUM\""),
            "Snowflake should uppercase measure col: {}",
            sql
        );
        assert!(
            sql.contains("\"ORDERS__TOTAL_REVENUE\""),
            "Snowflake should uppercase alias: {}",
            sql
        );
        assert!(
            sql.contains("\"PREAGG\""),
            "Snowflake should uppercase schema: {}",
            sql
        );
    }

    #[test]
    fn test_create_schema_ddl_all_dialects() {
        for dialect in all_dialects() {
            let ddl = dialect.create_schema_ddl("preagg");
            match dialect {
                Dialect::BigQuery => {
                    assert!(ddl.is_none(), "BigQuery should return None");
                }
                Dialect::ClickHouse => {
                    let sql = ddl.unwrap();
                    assert!(
                        sql.contains("CREATE DATABASE"),
                        "ClickHouse should use DATABASE: {}",
                        sql
                    );
                }
                _ => {
                    let sql = ddl.unwrap();
                    assert!(
                        sql.contains("CREATE SCHEMA"),
                        "{}: should use SCHEMA: {}",
                        dialect,
                        sql
                    );
                }
            }
        }
    }
}
