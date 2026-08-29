use crate::{
    metric_semantics::{self, MetricAggregation},
    model::Metric,
    report,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    Run,
    Series,
}

#[derive(Debug, Clone)]
pub struct MetricQueryOptions {
    pub scope: QueryScope,
    pub window: Option<String>,
    pub metrics: Vec<String>,
    pub metric_prefix: Option<String>,
    pub node: Option<String>,
    pub source: Option<String>,
    pub labels: Vec<(String, String)>,
    pub label_contains: Vec<(String, String)>,
    pub group_by: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct MetricQueryOutput {
    pub schema_version: u32,
    pub run_id: String,
    pub view: &'static str,
    pub scope: QueryScope,
    pub selection: QuerySelection,
    pub total_count: usize,
    pub truncated: bool,
    pub rows: Vec<MetricQueryRow>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryPresence {
    Both,
    Added,
    Removed,
}

#[derive(Debug, Serialize)]
pub struct NumericDiff {
    pub base: Option<f64>,
    pub candidate: Option<f64>,
    pub delta: Option<f64>,
    pub delta_percent: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct QueryDiffRow<T> {
    pub key: BTreeMap<String, String>,
    pub presence: QueryPresence,
    pub base: Option<T>,
    pub candidate: Option<T>,
    pub changes: BTreeMap<String, NumericDiff>,
}

#[derive(Debug, Serialize)]
pub struct QueryDiffOutput<T> {
    pub schema_version: u32,
    pub view: &'static str,
    pub base_run_id: String,
    pub candidate_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping: Option<&'static str>,
    pub total_count: usize,
    pub truncated: bool,
    pub rows: Vec<QueryDiffRow<T>>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct QuerySelection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    pub metrics: Vec<String>,
    pub metric_prefix: Option<String>,
    pub node: Option<String>,
    pub source: Option<String>,
    pub labels: Vec<QueryLabelFilter>,
    pub label_contains: Vec<QueryLabelFilter>,
    pub group_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryLabelFilter {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct MetricQueryRow {
    pub metric: String,
    pub unit: String,
    pub value: Option<f64>,
    pub aggregation: MetricAggregation,
    pub exact: bool,
    pub source_rows: usize,
    pub labels: BTreeMap<String, String>,
}

pub fn metric_query(
    run_id: String,
    metrics: Vec<Metric>,
    options: MetricQueryOptions,
) -> MetricQueryOutput {
    type Key = (String, String, BTreeMap<String, String>);
    let mut groups = BTreeMap::<Key, (MetricAggregation, Vec<f64>)>::new();
    for metric in metrics
        .into_iter()
        .filter(|metric| metric_matches(metric, &options))
    {
        let labels = output_labels(&metric, &options.group_by);
        let aggregation = if options.scope == QueryScope::Series {
            metric_semantics::time_series_aggregation(&metric)
        } else {
            metric_semantics::grouped_aggregation(&metric)
        };
        groups
            .entry((metric.name, metric.unit, labels))
            .or_insert_with(|| (aggregation, Vec::new()))
            .1
            .push(metric.value);
    }

    let mut non_mergeable_groups = 0;
    let total_count = groups.len();
    let mut rows = groups
        .into_iter()
        .map(|((metric, unit, labels), (aggregation, values))| {
            let value = metric_semantics::aggregate(&values, aggregation)
                .map(|value| round_metric_value(value, &unit));
            if value.is_none() {
                non_mergeable_groups += 1;
            }
            MetricQueryRow {
                metric,
                unit,
                value,
                aggregation,
                exact: value.is_some(),
                source_rows: values.len(),
                labels,
            }
        })
        .collect::<Vec<_>>();
    let warnings = (non_mergeable_groups > 0)
        .then(|| {
            format!(
                "{non_mergeable_groups} groups contain non-mergeable values; retain more labels or use additive components"
            )
        })
        .into_iter()
        .collect();

    MetricQueryOutput {
        schema_version: 1,
        run_id,
        view: "metrics",
        scope: options.scope,
        selection: QuerySelection {
            window: options.window,
            metrics: options.metrics,
            metric_prefix: options.metric_prefix,
            node: options.node,
            source: options.source,
            labels: options
                .labels
                .into_iter()
                .map(|(key, value)| QueryLabelFilter { key, value })
                .collect(),
            label_contains: options
                .label_contains
                .into_iter()
                .map(|(key, value)| QueryLabelFilter { key, value })
                .collect(),
            group_by: options.group_by,
        },
        total_count,
        truncated: total_count > options.limit,
        rows: {
            rows.truncate(options.limit);
            rows
        },
        warnings,
    }
}

pub fn metric_query_diff(
    base: MetricQueryOutput,
    candidate: MetricQueryOutput,
    limit: usize,
) -> QueryDiffOutput<MetricQueryRow> {
    type Key = (String, String, BTreeMap<String, String>);
    let base_run_id = base.run_id;
    let candidate_run_id = candidate.run_id;
    let window = candidate.selection.window.clone();
    let base_warnings = base.warnings;
    let candidate_warnings = candidate.warnings;
    let mut base = base
        .rows
        .into_iter()
        .map(|row| {
            (
                (row.metric.clone(), row.unit.clone(), row.labels.clone()),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .rows
        .into_iter()
        .map(|row| {
            (
                (row.metric.clone(), row.unit.clone(), row.labels.clone()),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut rows = keys
        .into_iter()
        .map(|(metric, unit, labels)| {
            let key = (metric.clone(), unit.clone(), labels.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            let mut output_key = labels;
            output_key.insert("metric".into(), metric);
            output_key.insert("unit".into(), unit);
            let changes = BTreeMap::from([(
                "value".into(),
                numeric_diff(
                    base.as_ref().and_then(|row| row.value),
                    candidate.as_ref().and_then(|row| row.value),
                ),
            )]);
            QueryDiffRow {
                key: output_key,
                presence: presence(&base, &candidate),
                base,
                candidate,
                changes,
            }
        })
        .collect::<Vec<_>>();
    sort_diff_rows(&mut rows, "value");
    let mut output = finish_diff(
        "metrics",
        base_run_id,
        candidate_run_id,
        None,
        rows,
        limit,
        combined_warnings(base_warnings, candidate_warnings),
    );
    output.window = window;
    output
}

fn metric_matches(metric: &Metric, options: &MetricQueryOptions) -> bool {
    if metric.timestamp.is_some() != (options.scope == QueryScope::Series) {
        return false;
    }
    if !options.metrics.is_empty() && !options.metrics.contains(&metric.name) {
        return false;
    }
    if let Some(prefix) = &options.metric_prefix
        && !metric.name.starts_with(prefix)
    {
        return false;
    }
    if let Some(node) = &options.node
        && metric.labels.get("node") != Some(node)
    {
        return false;
    }
    if let Some(source) = &options.source
        && metric.labels.get("collector") != Some(source)
        && metric.labels.get("isuscope.parser") != Some(source)
    {
        return false;
    }
    if options
        .labels
        .iter()
        .any(|(key, value)| metric.labels.get(key) != Some(value))
    {
        return false;
    }
    !options.label_contains.iter().any(|(key, needle)| {
        metric
            .labels
            .get(key)
            .is_none_or(|value| !value.contains(needle))
    })
}

fn output_labels(metric: &Metric, group_by: &[String]) -> BTreeMap<String, String> {
    if group_by.is_empty() {
        return metric.labels.clone();
    }
    const PROVENANCE: [&str; 4] = ["node", "collector", "engine", "isuscope.parser"];
    PROVENANCE
        .iter()
        .map(|value| value.to_string())
        .chain(group_by.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|key| metric.labels.get(&key).cloned().map(|value| (key, value)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct DatabaseQueryOptions {
    pub node: Option<String>,
    pub source: Option<String>,
    pub labels: Vec<(String, String)>,
    pub label_contains: Vec<(String, String)>,
    pub sql_shape: bool,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct DatabaseQueryOutput {
    pub schema_version: u32,
    pub run_id: String,
    pub view: &'static str,
    pub grouping: &'static str,
    pub total_count: usize,
    pub truncated: bool,
    pub rows: Vec<DatabaseQueryRow>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct DatabaseQueryRow {
    pub node: String,
    pub engine: String,
    pub source: String,
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_shape: Option<String>,
    pub digest_count: usize,
    pub digest_examples: Vec<String>,
    pub calls: f64,
    pub total_ms: f64,
    pub avg_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub lock_ms: f64,
    pub rows_sent: f64,
    pub rows_examined: f64,
    pub rows_examined_per_call: Option<f64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable: BTreeMap<String, String>,
}

pub fn database_query(
    run_id: String,
    metrics: Vec<Metric>,
    options: DatabaseQueryOptions,
) -> DatabaseQueryOutput {
    let filtered = metrics
        .into_iter()
        .filter(|metric| metric.timestamp.is_none())
        .filter(|metric| {
            options
                .node
                .as_ref()
                .is_none_or(|node| metric.labels.get("node") == Some(node))
        })
        .filter(|metric| {
            options
                .source
                .as_ref()
                .is_none_or(|source| metric.labels.get("collector") == Some(source))
        })
        .filter(|metric| {
            options
                .labels
                .iter()
                .all(|(key, value)| metric.labels.get(key) == Some(value))
        })
        .filter(|metric| {
            options.label_contains.iter().all(|(key, needle)| {
                metric
                    .labels
                    .get(key)
                    .is_some_and(|value| value.contains(needle))
            })
        })
        .collect::<Vec<_>>();
    let summaries = report::database_queries(&filtered);
    let mut rows = if options.sql_shape {
        group_database_shapes(summaries)
    } else {
        summaries
            .into_iter()
            .map(|summary| DatabaseQueryRow {
                node: summary.node,
                engine: summary.engine,
                source: summary.source,
                digest: summary.digest.clone(),
                sql_shape: None,
                digest_count: 1,
                digest_examples: vec![digest_example(&summary.digest)],
                calls: summary.calls,
                total_ms: summary.total_ms,
                avg_ms: summary.avg_ms,
                p95_ms: summary.p95_ms,
                lock_ms: summary.lock_ms,
                rows_sent: summary.rows_sent,
                rows_examined: summary.rows_examined,
                rows_examined_per_call: summary.rows_examined_per_call,
                unavailable: BTreeMap::new(),
            })
            .collect()
    };
    rows.iter_mut().for_each(round_database_query_row);
    rows.sort_by(|a, b| {
        b.total_ms
            .total_cmp(&a.total_ms)
            .then_with(|| a.digest.cmp(&b.digest))
    });
    let total_count = rows.len();
    let unavailable_p95 = rows
        .iter()
        .filter(|row| row.unavailable.contains_key("p95_ms"))
        .count();
    rows.truncate(options.limit);
    let warnings = (options.sql_shape && unavailable_p95 > 0)
        .then(|| {
            format!(
                "p95_ms is unavailable for {unavailable_p95} SQL-shape groups because scalar quantiles cannot be merged"
            )
        })
        .into_iter()
        .collect();
    DatabaseQueryOutput {
        schema_version: 1,
        run_id,
        view: "database",
        grouping: if options.sql_shape {
            "sql_shape"
        } else {
            "digest"
        },
        total_count,
        truncated: total_count > options.limit,
        rows,
        warnings,
    }
}

pub fn database_query_diff(
    base: DatabaseQueryOutput,
    candidate: DatabaseQueryOutput,
    limit: usize,
) -> QueryDiffOutput<DatabaseQueryRow> {
    type Key = (String, String, String, String);
    let grouping = candidate.grouping;
    let base_run_id = base.run_id;
    let candidate_run_id = candidate.run_id;
    let base_warnings = base.warnings;
    let candidate_warnings = candidate.warnings;
    let mut base = base
        .rows
        .into_iter()
        .map(|row| {
            (
                (
                    row.node.clone(),
                    row.engine.clone(),
                    row.source.clone(),
                    row.digest.clone(),
                ),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .rows
        .into_iter()
        .map(|row| {
            (
                (
                    row.node.clone(),
                    row.engine.clone(),
                    row.source.clone(),
                    row.digest.clone(),
                ),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut rows = keys
        .into_iter()
        .map(|(node, engine, source, digest)| {
            let key = (node.clone(), engine.clone(), source.clone(), digest.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            let changes = database_changes(base.as_ref(), candidate.as_ref());
            QueryDiffRow {
                key: BTreeMap::from([
                    ("node".into(), node),
                    ("engine".into(), engine),
                    ("source".into(), source),
                    (grouping.into(), digest),
                ]),
                presence: presence(&base, &candidate),
                base,
                candidate,
                changes,
            }
        })
        .collect::<Vec<_>>();
    sort_diff_rows(&mut rows, "total_ms");
    finish_diff(
        "database",
        base_run_id,
        candidate_run_id,
        Some(grouping),
        rows,
        limit,
        combined_warnings(base_warnings, candidate_warnings),
    )
}

#[derive(Debug, Clone)]
pub struct HttpQueryOptions {
    pub node: Option<String>,
    pub source: Option<String>,
    pub labels: Vec<(String, String)>,
    pub label_contains: Vec<(String, String)>,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct HttpQueryOutput {
    pub schema_version: u32,
    pub run_id: String,
    pub view: &'static str,
    pub total_count: usize,
    pub truncated: bool,
    pub rows: Vec<report::HttpRouteSummary>,
    pub warnings: Vec<String>,
}

pub fn http_query(
    run_id: String,
    metrics: Vec<Metric>,
    options: HttpQueryOptions,
) -> HttpQueryOutput {
    let filtered = metrics
        .into_iter()
        .filter(|metric| metric.timestamp.is_none())
        .filter(|metric| {
            options
                .node
                .as_ref()
                .is_none_or(|node| metric.labels.get("node") == Some(node))
        })
        .filter(|metric| {
            options
                .source
                .as_ref()
                .is_none_or(|source| metric.labels.get("collector") == Some(source))
        })
        .filter(|metric| {
            options
                .labels
                .iter()
                .all(|(key, value)| metric.labels.get(key) == Some(value))
        })
        .filter(|metric| {
            options.label_contains.iter().all(|(key, needle)| {
                metric
                    .labels
                    .get(key)
                    .is_some_and(|value| value.contains(needle))
            })
        })
        .collect::<Vec<_>>();
    let mut rows = report::http_routes(&filtered);
    rows.iter_mut().for_each(round_http_summary);
    let total_count = rows.len();
    rows.truncate(options.limit);
    HttpQueryOutput {
        schema_version: 1,
        run_id,
        view: "http",
        total_count,
        truncated: total_count > options.limit,
        rows,
        warnings: Vec::new(),
    }
}

pub fn http_query_diff(
    base: HttpQueryOutput,
    candidate: HttpQueryOutput,
    limit: usize,
) -> QueryDiffOutput<report::HttpRouteSummary> {
    type Key = (String, String, String);
    let base_run_id = base.run_id;
    let candidate_run_id = candidate.run_id;
    let mut base = base
        .rows
        .into_iter()
        .map(|row| {
            (
                (row.node.clone(), row.method.clone(), row.route.clone()),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .rows
        .into_iter()
        .map(|row| {
            (
                (row.node.clone(), row.method.clone(), row.route.clone()),
                row,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut rows = keys
        .into_iter()
        .map(|(node, method, route)| {
            let key = (node.clone(), method.clone(), route.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            let changes = http_changes(base.as_ref(), candidate.as_ref());
            QueryDiffRow {
                key: BTreeMap::from([
                    ("node".into(), node),
                    ("method".into(), method),
                    ("route".into(), route),
                ]),
                presence: presence(&base, &candidate),
                base,
                candidate,
                changes,
            }
        })
        .collect::<Vec<_>>();
    sort_diff_rows(&mut rows, "total_ms");
    finish_diff(
        "http",
        base_run_id,
        candidate_run_id,
        None,
        rows,
        limit,
        Vec::new(),
    )
}

fn database_changes(
    base: Option<&DatabaseQueryRow>,
    candidate: Option<&DatabaseQueryRow>,
) -> BTreeMap<String, NumericDiff> {
    [
        (
            "calls",
            base.map(|row| row.calls),
            candidate.map(|row| row.calls),
        ),
        (
            "total_ms",
            base.map(|row| row.total_ms),
            candidate.map(|row| row.total_ms),
        ),
        (
            "avg_ms",
            base.and_then(|row| row.avg_ms),
            candidate.and_then(|row| row.avg_ms),
        ),
        (
            "p95_ms",
            base.and_then(|row| row.p95_ms),
            candidate.and_then(|row| row.p95_ms),
        ),
        (
            "lock_ms",
            base.map(|row| row.lock_ms),
            candidate.map(|row| row.lock_ms),
        ),
        (
            "rows_sent",
            base.map(|row| row.rows_sent),
            candidate.map(|row| row.rows_sent),
        ),
        (
            "rows_examined",
            base.map(|row| row.rows_examined),
            candidate.map(|row| row.rows_examined),
        ),
        (
            "rows_examined_per_call",
            base.and_then(|row| row.rows_examined_per_call),
            candidate.and_then(|row| row.rows_examined_per_call),
        ),
    ]
    .into_iter()
    .map(|(name, base, candidate)| (name.into(), numeric_diff(base, candidate)))
    .collect()
}

fn http_changes(
    base: Option<&report::HttpRouteSummary>,
    candidate: Option<&report::HttpRouteSummary>,
) -> BTreeMap<String, NumericDiff> {
    [
        (
            "count",
            base.map(|row| row.count),
            candidate.map(|row| row.count),
        ),
        (
            "total_ms",
            base.and_then(|row| row.total_ms),
            candidate.and_then(|row| row.total_ms),
        ),
        (
            "avg_ms",
            base.and_then(|row| row.avg_ms),
            candidate.and_then(|row| row.avg_ms),
        ),
        (
            "p50_ms",
            base.and_then(|row| row.p50_ms),
            candidate.and_then(|row| row.p50_ms),
        ),
        (
            "p95_ms",
            base.and_then(|row| row.p95_ms),
            candidate.and_then(|row| row.p95_ms),
        ),
        (
            "p99_ms",
            base.and_then(|row| row.p99_ms),
            candidate.and_then(|row| row.p99_ms),
        ),
        (
            "errors",
            base.map(|row| row.errors),
            candidate.map(|row| row.errors),
        ),
        (
            "error_rate",
            base.and_then(|row| row.error_rate),
            candidate.and_then(|row| row.error_rate),
        ),
    ]
    .into_iter()
    .map(|(name, base, candidate)| (name.into(), numeric_diff(base, candidate)))
    .collect()
}

fn numeric_diff(base: Option<f64>, candidate: Option<f64>) -> NumericDiff {
    NumericDiff {
        base,
        candidate,
        delta: base
            .zip(candidate)
            .map(|(base, candidate)| round_to(candidate - base, 6)),
        delta_percent: base
            .zip(candidate)
            .filter(|(base, _)| *base != 0.0)
            .map(|(base, candidate)| round_to((candidate - base) / base * 100.0, 2)),
    }
}

fn presence<T>(base: &Option<T>, candidate: &Option<T>) -> QueryPresence {
    match (base.is_some(), candidate.is_some()) {
        (true, true) => QueryPresence::Both,
        (false, true) => QueryPresence::Added,
        (true, false) => QueryPresence::Removed,
        (false, false) => unreachable!("union key always has at least one row"),
    }
}

fn sort_diff_rows<T>(rows: &mut [QueryDiffRow<T>], preferred: &str) {
    rows.sort_by(|a, b| {
        diff_magnitude(b, preferred)
            .total_cmp(&diff_magnitude(a, preferred))
            .then_with(|| a.key.cmp(&b.key))
    });
}

fn diff_magnitude<T>(row: &QueryDiffRow<T>, preferred: &str) -> f64 {
    let Some(change) = row.changes.get(preferred) else {
        return 0.0;
    };
    change
        .delta
        .or(change.candidate)
        .or(change.base)
        .unwrap_or_default()
        .abs()
}

fn finish_diff<T>(
    view: &'static str,
    base_run_id: String,
    candidate_run_id: String,
    grouping: Option<&'static str>,
    mut rows: Vec<QueryDiffRow<T>>,
    limit: usize,
    warnings: Vec<String>,
) -> QueryDiffOutput<T> {
    let total_count = rows.len();
    rows.truncate(limit);
    QueryDiffOutput {
        schema_version: 1,
        view,
        base_run_id,
        candidate_run_id,
        window: None,
        grouping,
        total_count,
        truncated: total_count > rows.len(),
        rows,
        warnings,
    }
}

fn combined_warnings(base: Vec<String>, candidate: Vec<String>) -> Vec<String> {
    let base = base.into_iter().collect::<BTreeSet<_>>();
    let candidate = candidate.into_iter().collect::<BTreeSet<_>>();
    base.union(&candidate)
        .map(
            |warning| match (base.contains(warning), candidate.contains(warning)) {
                (true, true) => format!("base/candidate: {warning}"),
                (true, false) => format!("base: {warning}"),
                (false, true) => format!("candidate: {warning}"),
                (false, false) => unreachable!("warning came from the set union"),
            },
        )
        .collect()
}

fn round_metric_value(value: f64, unit: &str) -> f64 {
    let decimals = match unit {
        "queries" | "runs" | "checks" | "rows" | "bytes" | "samples" | "requests" => 0,
        "ms" | "percent" => 3,
        _ => 6,
    };
    round_to(value, decimals)
}

fn round_database_query_row(row: &mut DatabaseQueryRow) {
    row.calls = round_to(row.calls, 0);
    row.total_ms = round_to(row.total_ms, 3);
    row.avg_ms = row.avg_ms.map(|value| round_to(value, 3));
    row.p95_ms = row.p95_ms.map(|value| round_to(value, 3));
    row.lock_ms = round_to(row.lock_ms, 3);
    row.rows_sent = round_to(row.rows_sent, 0);
    row.rows_examined = round_to(row.rows_examined, 0);
    row.rows_examined_per_call = row.rows_examined_per_call.map(|value| round_to(value, 3));
}

pub fn round_database_summary(row: &mut report::DatabaseSummary) {
    row.calls = round_to(row.calls, 0);
    row.total_ms = round_to(row.total_ms, 3);
    row.avg_ms = row.avg_ms.map(|value| round_to(value, 3));
    row.p95_ms = row.p95_ms.map(|value| round_to(value, 3));
    row.lock_ms = round_to(row.lock_ms, 3);
    row.rows_sent = round_to(row.rows_sent, 0);
    row.rows_examined = round_to(row.rows_examined, 0);
    row.rows_examined_per_call = row.rows_examined_per_call.map(|value| round_to(value, 3));
}

pub fn round_http_summary(row: &mut report::HttpRouteSummary) {
    row.count = round_to(row.count, 0);
    row.total_ms = row.total_ms.map(|value| round_to(value, 3));
    row.avg_ms = row.avg_ms.map(|value| round_to(value, 3));
    row.min_ms = row.min_ms.map(|value| round_to(value, 3));
    row.p50_ms = row.p50_ms.map(|value| round_to(value, 3));
    row.p95_ms = row.p95_ms.map(|value| round_to(value, 3));
    row.p99_ms = row.p99_ms.map(|value| round_to(value, 3));
    row.max_ms = row.max_ms.map(|value| round_to(value, 3));
    row.errors = round_to(row.errors, 0);
    row.error_rate = row.error_rate.map(|value| round_to(value, 6));
    row.response_bytes = row.response_bytes.map(|value| round_to(value, 0));
    row.status_counts
        .values_mut()
        .for_each(|value| *value = round_to(*value, 0));
}

pub fn round_to(value: f64, decimals: u32) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn group_database_shapes(summaries: Vec<report::DatabaseSummary>) -> Vec<DatabaseQueryRow> {
    type Key = (String, String, String, String);
    let mut groups = BTreeMap::<Key, Vec<report::DatabaseSummary>>::new();
    for summary in summaries {
        let shape = sql_shape(&summary.digest);
        groups
            .entry((
                summary.node.clone(),
                summary.engine.clone(),
                summary.source.clone(),
                shape,
            ))
            .or_default()
            .push(summary);
    }
    groups
        .into_iter()
        .map(|((node, engine, source, shape), values)| {
            let calls = values.iter().map(|value| value.calls).sum::<f64>();
            let total_ms = values.iter().map(|value| value.total_ms).sum::<f64>();
            let lock_ms = values.iter().map(|value| value.lock_ms).sum::<f64>();
            let rows_sent = values.iter().map(|value| value.rows_sent).sum::<f64>();
            let rows_examined = values.iter().map(|value| value.rows_examined).sum::<f64>();
            let digests = values
                .iter()
                .map(|value| value.digest.clone())
                .collect::<BTreeSet<_>>();
            let p95_ms = (digests.len() == 1).then(|| values[0].p95_ms).flatten();
            let mut unavailable = BTreeMap::new();
            if digests.len() > 1 {
                unavailable.insert(
                    "p95_ms".into(),
                    "scalar quantiles cannot be merged across digests".into(),
                );
            }
            DatabaseQueryRow {
                node,
                engine,
                source,
                digest: shape.clone(),
                sql_shape: Some(shape),
                digest_count: digests.len(),
                digest_examples: digests
                    .into_iter()
                    .take(3)
                    .map(|digest| digest_example(&digest))
                    .collect(),
                calls,
                total_ms,
                avg_ms: divide(total_ms, calls),
                p95_ms,
                lock_ms,
                rows_sent,
                rows_examined,
                rows_examined_per_call: divide(rows_examined, calls),
                unavailable,
            }
        })
        .collect()
}

fn divide(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator != 0.0).then_some(numerator / denominator)
}

pub fn sql_shape(digest: &str) -> String {
    let chars = digest.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(digest.len());
    let mut index = 0;
    while index < chars.len() {
        if let Some(end) = starts_values_placeholder_tuples(&chars, index) {
            output.push_str("values (?tuple+)");
            index = end;
            continue;
        }
        if starts_in_placeholder_list(&chars, index).is_some_and(|end| {
            output.push_str("in (?+)");
            index = end;
            true
        }) {
            continue;
        }
        output.extend(chars[index].to_lowercase());
        index += 1;
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn starts_values_placeholder_tuples(chars: &[char], start: usize) -> Option<usize> {
    const TOKEN: [char; 6] = ['v', 'a', 'l', 'u', 'e', 's'];
    if start > 0 && is_identifier(chars[start - 1])
        || chars
            .get(start + TOKEN.len())
            .is_some_and(|value| is_identifier(*value))
        || TOKEN.iter().enumerate().any(|(offset, expected)| {
            chars
                .get(start + offset)
                .is_none_or(|value| !value.eq_ignore_ascii_case(expected))
        })
    {
        return None;
    }
    let mut index = start + TOKEN.len();
    skip_space(chars, &mut index);
    let (mut end, arity) = placeholder_tuple(chars, index)?;
    loop {
        let mut next = end;
        skip_space(chars, &mut next);
        if chars.get(next) != Some(&',') {
            return Some(end);
        }
        next += 1;
        skip_space(chars, &mut next);
        let Some((next_end, next_arity)) = placeholder_tuple(chars, next) else {
            return placeholder_only_tail(&chars[next..]).then_some(chars.len());
        };
        if next_arity != arity {
            return None;
        }
        end = next_end;
    }
}

fn placeholder_tuple(chars: &[char], start: usize) -> Option<(usize, usize)> {
    if chars.get(start) != Some(&'(') {
        return None;
    }
    let mut index = start + 1;
    let mut arity = 0;
    loop {
        skip_space(chars, &mut index);
        if chars.get(index) != Some(&'?') {
            return None;
        }
        arity += 1;
        index += 1;
        skip_space(chars, &mut index);
        match chars.get(index) {
            Some(',') => index += 1,
            Some(')') => return Some((index + 1, arity)),
            _ => return None,
        }
    }
}

fn placeholder_only_tail(chars: &[char]) -> bool {
    !chars.is_empty()
        && chars
            .iter()
            .all(|value| value.is_whitespace() || matches!(value, '(' | ')' | ',' | '?'))
}

fn digest_example(digest: &str) -> String {
    const LIMIT: usize = 160;
    let mut chars = digest.chars();
    let prefix = chars.by_ref().take(LIMIT).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn starts_in_placeholder_list(chars: &[char], start: usize) -> Option<usize> {
    if chars
        .get(start)
        .is_none_or(|value| !value.eq_ignore_ascii_case(&'i'))
        || chars
            .get(start + 1)
            .is_none_or(|value| !value.eq_ignore_ascii_case(&'n'))
        || start > 0 && is_identifier(chars[start - 1])
        || chars
            .get(start + 2)
            .is_some_and(|value| is_identifier(*value))
    {
        return None;
    }
    let mut index = start + 2;
    skip_space(chars, &mut index);
    if chars.get(index) != Some(&'(') {
        return None;
    }
    index += 1;
    loop {
        skip_space(chars, &mut index);
        if chars.get(index) != Some(&'?') {
            return None;
        }
        index += 1;
        skip_space(chars, &mut index);
        match chars.get(index) {
            Some(',') => index += 1,
            Some(')') => return Some(index + 1),
            _ => return None,
        }
    }
}

fn skip_space(chars: &[char], index: &mut usize) {
    while chars.get(*index).is_some_and(|value| value.is_whitespace()) {
        *index += 1;
    }
}

fn is_identifier(value: char) -> bool {
    value.is_alphanumeric() || value == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_shape_collapses_variable_length_in_lists() {
        assert_eq!(
            sql_shape("UPDATE t SET n = ? WHERE id IN (?,?,?,?,?)"),
            "update t set n = ? where id in (?+)"
        );
        assert_eq!(
            sql_shape("select * from t where id in (?)"),
            "select * from t where id in (?+)"
        );
        assert_eq!(sql_shape("select begin from t"), "select begin from t");
        assert_eq!(
            sql_shape("INSERT INTO t (a,b) VALUES (?,?),(?,?),(?,?)"),
            "insert into t (a,b) values (?tuple+)"
        );
        assert_eq!(
            sql_shape("insert into t (a,b) values (?,?),(?,?),(?,?"),
            "insert into t (a,b) values (?tuple+)"
        );
    }

    #[test]
    fn metric_query_preserves_provenance_when_grouping() {
        let metrics = ["mysql-log-delta", "slp"]
            .into_iter()
            .map(|collector| Metric {
                name: "db.query.calls".into(),
                value: 10.0,
                unit: "queries".into(),
                timestamp: None,
                labels: BTreeMap::from([
                    ("collector".into(), collector.into()),
                    ("digest".into(), "select ?".into()),
                ]),
            })
            .collect();
        let output = metric_query(
            "run".into(),
            metrics,
            MetricQueryOptions {
                scope: QueryScope::Run,
                window: None,
                metrics: vec!["db.query.calls".into()],
                metric_prefix: None,
                node: None,
                source: None,
                labels: Vec::new(),
                label_contains: Vec::new(),
                group_by: vec!["digest".into()],
                limit: 100,
            },
        );
        assert_eq!(output.rows.len(), 2);
        assert!(output.rows.iter().all(|row| row.value == Some(10.0)));
    }

    #[test]
    fn metric_diff_joins_all_rows_before_limiting() {
        fn rows(values: &[(&str, f64)]) -> Vec<Metric> {
            values
                .iter()
                .map(|(scenario, value)| Metric {
                    name: "benchmark.scenario.success".into(),
                    value: *value,
                    unit: "runs".into(),
                    timestamp: None,
                    labels: BTreeMap::from([("scenario".into(), (*scenario).into())]),
                })
                .collect()
        }
        let options = MetricQueryOptions {
            scope: QueryScope::Run,
            window: None,
            metrics: vec!["benchmark.scenario.success".into()],
            metric_prefix: None,
            node: None,
            source: None,
            labels: Vec::new(),
            label_contains: Vec::new(),
            group_by: vec!["scenario".into()],
            limit: usize::MAX,
        };
        let base = metric_query(
            "base".into(),
            rows(&[("a", 100.0), ("b", 1.0)]),
            options.clone(),
        );
        let candidate = metric_query(
            "candidate".into(),
            rows(&[("a", 101.0), ("c", 50.0)]),
            options,
        );
        let diff = metric_query_diff(base, candidate, 1);
        assert_eq!(diff.total_count, 3);
        assert!(diff.truncated);
        assert_eq!(diff.rows[0].presence, QueryPresence::Added);
        assert_eq!(
            diff.rows[0].key.get("scenario").map(String::as_str),
            Some("c")
        );
    }
}
