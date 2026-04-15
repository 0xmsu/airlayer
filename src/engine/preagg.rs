//! Pre-aggregation: rollup resolution, SQL generation, coverage checking.

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
