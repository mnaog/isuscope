use crate::model::Metric;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricAggregation {
    Sum,
    Average,
    Minimum,
    Maximum,
    MaxOfQuantile,
    NonMergeable,
}

pub fn time_series_aggregation(metric: &Metric) -> MetricAggregation {
    if is_quantile(metric) {
        return MetricAggregation::MaxOfQuantile;
    }
    match metric.name.as_str() {
        name if is_additive(name) => MetricAggregation::Sum,
        name if name.ends_with("_min") => MetricAggregation::Minimum,
        name if name.ends_with("_max") => MetricAggregation::Maximum,
        _ => MetricAggregation::Average,
    }
}

pub fn grouped_aggregation(metric: &Metric) -> MetricAggregation {
    if is_quantile(metric)
        || metric.name.ends_with("_mean")
        || metric.name == "db.query.p95_duration"
    {
        return MetricAggregation::NonMergeable;
    }
    match metric.name.as_str() {
        name if is_additive(name) => MetricAggregation::Sum,
        name if name.ends_with("_min") => MetricAggregation::Minimum,
        name if name.ends_with("_max") => MetricAggregation::Maximum,
        _ => MetricAggregation::NonMergeable,
    }
}

pub fn aggregate(values: &[f64], aggregation: MetricAggregation) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    match aggregation {
        MetricAggregation::Sum => Some(values.iter().sum()),
        MetricAggregation::Average => Some(values.iter().sum::<f64>() / values.len() as f64),
        MetricAggregation::Minimum => values.iter().copied().reduce(f64::min),
        MetricAggregation::Maximum | MetricAggregation::MaxOfQuantile => {
            values.iter().copied().reduce(f64::max)
        }
        MetricAggregation::NonMergeable if values.len() == 1 => Some(values[0]),
        MetricAggregation::NonMergeable => None,
    }
}

fn is_quantile(metric: &Metric) -> bool {
    metric.labels.contains_key("quantile") || metric.name.contains("p95")
}

fn is_additive(name: &str) -> bool {
    matches!(
        name,
        "http.requests"
            | "http.errors"
            | "http.response_bytes"
            | "http.connection_reused_requests"
            | "http.request_duration_sum"
            | "http.upstream_duration_sum"
            | "db.query.calls"
            | "db.query.total_duration"
            | "db.query.lock_duration"
            | "db.query.rows_sent"
            | "db.query.rows_examined"
            | "cpu.sample_count"
            | "benchmark.dns.failed"
            | "benchmark.dns.resolved"
            | "benchmark.scenario.failure"
            | "benchmark.scenario.success"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn metric(name: &str) -> Metric {
        Metric {
            name: name.into(),
            value: 1.0,
            unit: String::new(),
            timestamp: None,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn database_counters_and_durations_are_additive() {
        for name in [
            "db.query.calls",
            "db.query.total_duration",
            "db.query.lock_duration",
            "db.query.rows_sent",
            "db.query.rows_examined",
        ] {
            assert_eq!(
                time_series_aggregation(&metric(name)),
                MetricAggregation::Sum,
                "{name}"
            );
        }
    }

    #[test]
    fn grouped_quantiles_are_not_fabricated() {
        let aggregation = grouped_aggregation(&metric("db.query.p95_duration"));
        assert_eq!(aggregation, MetricAggregation::NonMergeable);
        assert_eq!(aggregate(&[1.0, 10.0], aggregation), None);
    }

    #[test]
    fn unknown_grouped_metrics_are_not_implicitly_averaged() {
        let aggregation = grouped_aggregation(&metric("custom.latency"));
        assert_eq!(aggregation, MetricAggregation::NonMergeable);
        assert_eq!(aggregate(&[1.0, 2.0], aggregation), None);
    }
}
