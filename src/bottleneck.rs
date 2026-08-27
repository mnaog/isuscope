use crate::model::Metric;
use std::collections::{BTreeMap, BTreeSet};

const DEFAULT_LIMIT: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Bottleneck {
    pub category: &'static str,
    pub node: String,
    pub target: String,
    pub evidence: String,
    pub source: &'static str,
    /// A category-local 0..1 score. Scores are deliberately not physical units.
    pub severity: f64,
    pub verify_metric: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub category: &'static str,
    pub available: bool,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub candidates: Vec<Bottleneck>,
    pub coverage: Vec<Coverage>,
}

#[derive(Default)]
struct HttpStats {
    requests: f64,
    p95: Option<f64>,
}
#[derive(Default)]
struct DbStats {
    calls: f64,
    total: f64,
}

/// Produces cross-category suspicions without adding unlike units together.
///
/// Each category is normalized by its dominant observation. We first retain the
/// strongest candidate from every observed category, then fill the remaining
/// slots by category-local severity. This keeps a hot endpoint from hiding a
/// saturated disk (and vice versa), while still returning a short work queue.
pub fn rank(metrics: &[Metric]) -> Report {
    let mut by_category = BTreeMap::<&'static str, Vec<Bottleneck>>::new();
    http(metrics, &mut by_category);
    database(metrics, &mut by_category);
    cpu(metrics, &mut by_category);
    host(metrics, &mut by_category);

    for values in by_category.values_mut() {
        let max = values.iter().map(|v| v.severity).fold(0.0_f64, f64::max);
        if max > 0.0 {
            for value in values.iter_mut() {
                value.severity /= max;
            }
        }
        values.sort_by(|a, b| {
            b.severity
                .total_cmp(&a.severity)
                .then_with(|| a.target.cmp(&b.target))
        });
    }

    let categories = ["http", "database", "cpu", "host"];
    let coverage = categories
        .iter()
        .map(|category| Coverage {
            category,
            available: by_category.get(category).is_some_and(|v| !v.is_empty()),
        })
        .collect();
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for category in categories {
        if let Some(value) = by_category.get(category).and_then(|v| v.first()) {
            seen.insert((value.category, value.node.clone(), value.target.clone()));
            candidates.push(value.clone());
        }
    }
    let mut rest: Vec<_> = by_category
        .into_values()
        .flatten()
        .filter(|v| !seen.contains(&(v.category, v.node.clone(), v.target.clone())))
        .collect();
    rest.sort_by(|a, b| {
        b.severity
            .total_cmp(&a.severity)
            .then_with(|| a.category.cmp(b.category))
            .then_with(|| a.target.cmp(&b.target))
    });
    candidates.extend(rest);
    candidates.truncate(DEFAULT_LIMIT);
    Report {
        candidates,
        coverage,
    }
}

fn http(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    let mut stats = BTreeMap::<(String, String, String), HttpStats>::new();
    for m in metrics {
        let Some(route) = m.labels.get("route") else {
            continue;
        };
        let key = (label(m, "node"), label(m, "method"), route.clone());
        let s = stats.entry(key).or_default();
        if m.name == "http.requests" {
            s.requests += m.value;
        }
        if m.name == "http.request_duration"
            && m.labels.get("quantile").is_some_and(|q| q == "0.95")
        {
            s.p95 = Some(m.value);
        }
    }
    for ((node, method, route), s) in stats {
        if let Some(p95) = s.p95 {
            if s.requests > 0.0 && valid(p95) {
                out.entry("http").or_default().push(Bottleneck {
                    category: "http",
                    node,
                    target: format!("{method} {route}"),
                    evidence: format!(
                        "requests={:.0} p95={p95:.2}ms impact={:.2}ms",
                        s.requests,
                        s.requests * p95
                    ),
                    source: "alp/access-log",
                    severity: s.requests * p95,
                    verify_metric: "http.request_duration{quantile=0.95}",
                });
            }
        }
    }
}

fn database(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    let mut stats = BTreeMap::<(String, String, String), DbStats>::new();
    for m in metrics {
        if !matches!(
            m.name.as_str(),
            "db.query.calls" | "db.query.total_duration"
        ) {
            continue;
        }
        let Some(digest) = m.labels.get("digest") else {
            continue;
        };
        let key = (label(m, "node"), label(m, "engine"), digest.clone());
        let s = stats.entry(key).or_default();
        if m.name == "db.query.calls" {
            s.calls += m.value
        } else {
            s.total += m.value
        }
    }
    for ((node, engine, digest), s) in stats {
        if valid(s.total) && s.total > 0.0 {
            out.entry("database").or_default().push(Bottleneck {
                category: "database",
                node,
                target: format!("{engine} {digest}"),
                evidence: format!(
                    "total={:.2}ms calls={:.0} mean={:.2}ms",
                    s.total,
                    s.calls,
                    if s.calls > 0.0 {
                        s.total / s.calls
                    } else {
                        0.0
                    }
                ),
                source: "slp/pg_stat_statements",
                severity: s.total,
                verify_metric: "db.query.total_duration",
            });
        }
    }
}

fn cpu(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    for m in metrics {
        if matches!(m.name.as_str(), "cpu.sample_percent" | "cpu.samples")
            && valid(m.value)
            && m.value > 0.0
        {
            out.entry("cpu").or_default().push(Bottleneck {
                category: "cpu",
                node: label(m, "node"),
                target: format!("{} {}", label(m, "binary"), label(m, "symbol")),
                evidence: format!("sample_share={:.2}%", m.value),
                source: "perf",
                severity: m.value,
                verify_metric: "cpu.sample_percent",
            });
        }
    }
}

fn host(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    for m in metrics {
        let (target, source, verify, weight) = match m.name.as_str() {
            "host.cpu_percent" => ("cpu".into(), "sysstat", "host.cpu_percent", m.value / 100.0),
            "host.disk_util_percent" => (
                format!("disk {} util", label(m, "device")),
                "sysstat",
                "host.disk_util_percent",
                m.value / 100.0,
            ),
            "host.disk_await" => (
                format!("disk {} await", label(m, "device")),
                "sysstat",
                "host.disk_await",
                m.value / 20.0,
            ),
            _ => continue,
        };
        if valid(weight) && weight > 0.0 {
            out.entry("host").or_default().push(Bottleneck {
                category: "host",
                node: label(m, "node"),
                target,
                evidence: format!("{}={:.2}{}", m.name, m.value, m.unit),
                source,
                severity: weight,
                verify_metric: verify,
            });
        }
    }
}
fn label(m: &Metric, key: &str) -> String {
    m.labels.get(key).cloned().unwrap_or_else(|| "-".into())
}
fn valid(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(name: &str, value: f64, labels: &[(&str, &str)]) -> Metric {
        Metric {
            name: name.into(),
            value,
            unit: "ms".into(),
            timestamp: None,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
    #[test]
    fn keeps_each_observed_category_before_filling_top_five() {
        let metrics = vec![
            m("http.requests", 100., &[("route", "/a"), ("method", "GET")]),
            m(
                "http.request_duration",
                10.,
                &[("route", "/a"), ("method", "GET"), ("quantile", "0.95")],
            ),
            m(
                "db.query.calls",
                2.,
                &[("digest", "select ?"), ("engine", "mysql")],
            ),
            m(
                "db.query.total_duration",
                200.,
                &[("digest", "select ?"), ("engine", "mysql")],
            ),
            m(
                "cpu.sample_percent",
                90.,
                &[("symbol", "work"), ("binary", "app")],
            ),
            m("host.disk_await", 30., &[("device", "sda")]),
        ];
        let r = rank(&metrics);
        assert_eq!(r.candidates.len(), 4);
        assert!(r.coverage.iter().all(|c| c.available));
        assert_eq!(
            r.candidates.iter().map(|c| c.category).collect::<Vec<_>>(),
            vec!["http", "database", "cpu", "host"]
        );
    }
}
