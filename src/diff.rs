use crate::{
    model::{RunManifest, Transition},
    report::{
        CoverageSummary, CpuSummary, DatabaseSummary, HostSummary, HttpRouteSummary, RunDiagnostics,
    },
};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

const COMPACT_LIMIT: usize = 20;

#[derive(Debug, Serialize)]
pub struct RunDiff {
    pub schema_version: u32,
    pub base: DiffRun,
    pub candidate: DiffRun,
    pub score: ScoreDiff,
    pub coverage: DiffSection<CoverageDiff>,
    pub http: DiffSection<HttpDiff>,
    pub database: DiffSection<DatabaseDiff>,
    pub cpu: DiffSection<CpuDiff>,
    pub host: DiffSection<HostDiff>,
    pub transitions: DiffSection<TransitionDiff>,
}

#[derive(Debug, Serialize)]
pub struct DiffRun {
    pub id: String,
    pub started_at: String,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub mode: String,
    pub state: String,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    pub hypothesis: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ScoreDiff {
    pub base: Option<i64>,
    pub candidate: Option<i64>,
    pub delta: Option<i64>,
    pub delta_percent: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DiffSection<T> {
    pub total_count: usize,
    pub truncated: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Both,
    Added,
    Removed,
}

impl Presence {
    fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct NumericDiff {
    pub base: Option<f64>,
    pub candidate: Option<f64>,
    pub delta: Option<f64>,
    pub delta_percent: Option<f64>,
}

impl NumericDiff {
    fn new(base: Option<f64>, candidate: Option<f64>) -> Self {
        let delta = base
            .zip(candidate)
            .map(|(base, candidate)| candidate - base);
        let delta_percent = base
            .zip(candidate)
            .filter(|(base, _)| *base != 0.0)
            .map(|(base, candidate)| (candidate - base) / base * 100.0);
        Self {
            base,
            candidate,
            delta,
            delta_percent,
        }
    }

    fn magnitude(&self) -> f64 {
        self.delta
            .map(f64::abs)
            .or_else(|| self.candidate.map(f64::abs))
            .or_else(|| self.base.map(f64::abs))
            .unwrap_or_default()
    }
}

#[derive(Debug, Serialize)]
pub struct CoverageDiff {
    pub section: String,
    pub node: String,
    pub collector: String,
    pub phase: String,
    pub presence: Presence,
    pub base_status: Option<String>,
    pub candidate_status: Option<String>,
    pub base_missing_metrics: Vec<String>,
    pub candidate_missing_metrics: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HttpDiff {
    pub node: String,
    pub method: String,
    pub route: String,
    pub presence: Presence,
    pub count: NumericDiff,
    pub total_ms: NumericDiff,
    pub avg_ms: NumericDiff,
    pub p50_ms: NumericDiff,
    pub p95_ms: NumericDiff,
    pub p99_ms: NumericDiff,
    pub errors: NumericDiff,
    pub error_rate: NumericDiff,
    pub response_bytes: NumericDiff,
    pub status_counts: BTreeMap<String, NumericDiff>,
}

#[derive(Debug, Serialize)]
pub struct DatabaseDiff {
    pub node: String,
    pub engine: String,
    pub digest: String,
    pub source: String,
    pub presence: Presence,
    pub calls: NumericDiff,
    pub total_ms: NumericDiff,
    pub avg_ms: NumericDiff,
    pub p95_ms: NumericDiff,
    pub lock_ms: NumericDiff,
    pub rows_sent: NumericDiff,
    pub rows_examined: NumericDiff,
    pub rows_examined_per_call: NumericDiff,
}

#[derive(Debug, Serialize)]
pub struct CpuDiff {
    pub node: String,
    pub process: String,
    pub binary: String,
    pub symbol: String,
    pub source: String,
    pub presence: Presence,
    pub sample_percent: NumericDiff,
}

#[derive(Debug, Serialize)]
pub struct HostDiff {
    pub node: String,
    pub metric: String,
    pub target: String,
    pub source: String,
    pub presence: Presence,
    pub base_unit: Option<String>,
    pub candidate_unit: Option<String>,
    pub average: NumericDiff,
    pub peak: NumericDiff,
    pub base_peak_at: Option<String>,
    pub candidate_peak_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TransitionDiff {
    pub from_route: String,
    pub to_route: String,
    pub presence: Presence,
    pub count: NumericDiff,
    pub p50_ms: NumericDiff,
    pub p95_ms: NumericDiff,
}

pub fn build(base: RunDiagnostics, candidate: RunDiagnostics) -> RunDiff {
    let base_run = diff_run(&base.run);
    let candidate_run = diff_run(&candidate.run);
    let score = score_diff(base.run.benchmark.score, candidate.run.benchmark.score);
    RunDiff {
        schema_version: 1,
        base: base_run,
        candidate: candidate_run,
        score,
        coverage: coverage_diff(base.coverage, candidate.coverage),
        http: http_diff(base.http, candidate.http),
        database: database_diff(base.database, candidate.database),
        cpu: cpu_diff(base.cpu, candidate.cpu),
        host: host_diff(base.host, candidate.host),
        transitions: transition_diff(base.transitions, candidate.transitions),
    }
}

fn diff_run(run: &RunManifest) -> DiffRun {
    DiffRun {
        id: run.id.clone(),
        started_at: run.started_at.to_rfc3339(),
        commit_hash: run.source.commit_hash.clone(),
        dirty: run.source.dirty,
        mode: run.mode.as_str().into(),
        state: run.state.as_str().into(),
        score: run.benchmark.score,
        passed: run.benchmark.passed,
        hypothesis: run.hypothesis.clone(),
        tags: run.tags.clone(),
    }
}

fn score_diff(base: Option<i64>, candidate: Option<i64>) -> ScoreDiff {
    ScoreDiff {
        base,
        candidate,
        delta: base
            .zip(candidate)
            .map(|(base, candidate)| candidate - base),
        delta_percent: base
            .zip(candidate)
            .filter(|(base, _)| *base != 0)
            .map(|(base, candidate)| (candidate - base) as f64 / base as f64 * 100.0),
    }
}

fn presence<T>(base: &Option<T>, candidate: &Option<T>) -> Presence {
    match (base.is_some(), candidate.is_some()) {
        (true, true) => Presence::Both,
        (false, true) => Presence::Added,
        (true, false) => Presence::Removed,
        (false, false) => unreachable!("union keys always have at least one value"),
    }
}

fn compact<T>(mut items: Vec<T>, compare: impl Fn(&T, &T) -> std::cmp::Ordering) -> DiffSection<T> {
    items.sort_by(compare);
    let total_count = items.len();
    items.truncate(COMPACT_LIMIT);
    DiffSection {
        total_count,
        truncated: total_count > items.len(),
        items,
    }
}

fn coverage_diff(
    base: Vec<CoverageSummary>,
    candidate: Vec<CoverageSummary>,
) -> DiffSection<CoverageDiff> {
    type Key = (String, String, String, String);
    let mut base = base
        .into_iter()
        .map(|item| {
            (
                (
                    item.section.clone(),
                    item.node.clone(),
                    item.collector.clone(),
                    item.phase.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| {
            (
                (
                    item.section.clone(),
                    item.node.clone(),
                    item.collector.clone(),
                    item.phase.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(section, node, collector, phase)| {
            let key = (
                section.clone(),
                node.clone(),
                collector.clone(),
                phase.clone(),
            );
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            CoverageDiff {
                section,
                node,
                collector,
                phase,
                presence: presence(&base, &candidate),
                base_status: base.as_ref().map(|item| item.status.clone()),
                candidate_status: candidate.as_ref().map(|item| item.status.clone()),
                base_missing_metrics: base.map_or_else(Vec::new, |item| item.missing_metrics),
                candidate_missing_metrics: candidate
                    .map_or_else(Vec::new, |item| item.missing_metrics),
            }
        })
        .collect();
    compact(items, |a, b| {
        let a_changed = a.base_status != a.candidate_status
            || a.base_missing_metrics != a.candidate_missing_metrics;
        let b_changed = b.base_status != b.candidate_status
            || b.base_missing_metrics != b.candidate_missing_metrics;
        b_changed
            .cmp(&a_changed)
            .then_with(|| a.section.cmp(&b.section))
            .then_with(|| a.node.cmp(&b.node))
            .then_with(|| a.collector.cmp(&b.collector))
    })
}

fn http_diff(
    base: Vec<HttpRouteSummary>,
    candidate: Vec<HttpRouteSummary>,
) -> DiffSection<HttpDiff> {
    type Key = (String, String, String);
    let mut base = base
        .into_iter()
        .map(|item| {
            (
                (item.node.clone(), item.method.clone(), item.route.clone()),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| {
            (
                (item.node.clone(), item.method.clone(), item.route.clone()),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(node, method, route)| {
            let key = (node.clone(), method.clone(), route.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            let status_keys = base
                .iter()
                .flat_map(|item| item.status_counts.keys())
                .chain(candidate.iter().flat_map(|item| item.status_counts.keys()))
                .cloned()
                .collect::<BTreeSet<_>>();
            let status_counts = status_keys
                .into_iter()
                .map(|status| {
                    let values = NumericDiff::new(
                        base.as_ref()
                            .and_then(|item| item.status_counts.get(&status).copied()),
                        candidate
                            .as_ref()
                            .and_then(|item| item.status_counts.get(&status).copied()),
                    );
                    (status, values)
                })
                .collect();
            HttpDiff {
                node,
                method,
                route,
                presence: presence(&base, &candidate),
                count: NumericDiff::new(
                    base.as_ref().map(|item| item.count),
                    candidate.as_ref().map(|item| item.count),
                ),
                total_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.total_ms),
                    candidate.as_ref().and_then(|item| item.total_ms),
                ),
                avg_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.avg_ms),
                    candidate.as_ref().and_then(|item| item.avg_ms),
                ),
                p50_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p50_ms),
                    candidate.as_ref().and_then(|item| item.p50_ms),
                ),
                p95_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p95_ms),
                    candidate.as_ref().and_then(|item| item.p95_ms),
                ),
                p99_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p99_ms),
                    candidate.as_ref().and_then(|item| item.p99_ms),
                ),
                errors: NumericDiff::new(
                    base.as_ref().map(|item| item.errors),
                    candidate.as_ref().map(|item| item.errors),
                ),
                error_rate: NumericDiff::new(
                    base.as_ref().and_then(|item| item.error_rate),
                    candidate.as_ref().and_then(|item| item.error_rate),
                ),
                response_bytes: NumericDiff::new(
                    base.as_ref().and_then(|item| item.response_bytes),
                    candidate.as_ref().and_then(|item| item.response_bytes),
                ),
                status_counts,
            }
        })
        .collect();
    compact(items, |a, b| {
        b.total_ms
            .magnitude()
            .total_cmp(&a.total_ms.magnitude())
            .then_with(|| b.count.magnitude().total_cmp(&a.count.magnitude()))
            .then_with(|| a.route.cmp(&b.route))
    })
}

fn database_diff(
    base: Vec<DatabaseSummary>,
    candidate: Vec<DatabaseSummary>,
) -> DiffSection<DatabaseDiff> {
    type Key = (String, String, String, String);
    let mut base = base
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.engine.clone(),
                    item.digest.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.engine.clone(),
                    item.digest.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(node, engine, digest, source)| {
            let key = (node.clone(), engine.clone(), digest.clone(), source.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            DatabaseDiff {
                node,
                engine,
                digest,
                source,
                presence: presence(&base, &candidate),
                calls: NumericDiff::new(
                    base.as_ref().map(|item| item.calls),
                    candidate.as_ref().map(|item| item.calls),
                ),
                total_ms: NumericDiff::new(
                    base.as_ref().map(|item| item.total_ms),
                    candidate.as_ref().map(|item| item.total_ms),
                ),
                avg_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.avg_ms),
                    candidate.as_ref().and_then(|item| item.avg_ms),
                ),
                p95_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p95_ms),
                    candidate.as_ref().and_then(|item| item.p95_ms),
                ),
                lock_ms: NumericDiff::new(
                    base.as_ref().map(|item| item.lock_ms),
                    candidate.as_ref().map(|item| item.lock_ms),
                ),
                rows_sent: NumericDiff::new(
                    base.as_ref().map(|item| item.rows_sent),
                    candidate.as_ref().map(|item| item.rows_sent),
                ),
                rows_examined: NumericDiff::new(
                    base.as_ref().map(|item| item.rows_examined),
                    candidate.as_ref().map(|item| item.rows_examined),
                ),
                rows_examined_per_call: NumericDiff::new(
                    base.as_ref().and_then(|item| item.rows_examined_per_call),
                    candidate
                        .as_ref()
                        .and_then(|item| item.rows_examined_per_call),
                ),
            }
        })
        .collect();
    compact(items, |a, b| {
        b.total_ms
            .magnitude()
            .total_cmp(&a.total_ms.magnitude())
            .then_with(|| a.digest.cmp(&b.digest))
    })
}

fn cpu_diff(base: Vec<CpuSummary>, candidate: Vec<CpuSummary>) -> DiffSection<CpuDiff> {
    type Key = (String, String, String, String, String);
    let mut base = base
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.process.clone(),
                    item.binary.clone(),
                    item.symbol.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.process.clone(),
                    item.binary.clone(),
                    item.symbol.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(node, process, binary, symbol, source)| {
            let key = (
                node.clone(),
                process.clone(),
                binary.clone(),
                symbol.clone(),
                source.clone(),
            );
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            CpuDiff {
                node,
                process,
                binary,
                symbol,
                source,
                presence: presence(&base, &candidate),
                sample_percent: NumericDiff::new(
                    base.as_ref().map(|item| item.sample_percent),
                    candidate.as_ref().map(|item| item.sample_percent),
                ),
            }
        })
        .collect();
    compact(items, |a, b| {
        b.sample_percent
            .magnitude()
            .total_cmp(&a.sample_percent.magnitude())
            .then_with(|| a.symbol.cmp(&b.symbol))
    })
}

fn host_diff(base: Vec<HostSummary>, candidate: Vec<HostSummary>) -> DiffSection<HostDiff> {
    type Key = (String, String, String, String);
    let mut base = base
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.metric.clone(),
                    item.target.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| {
            (
                (
                    item.node.clone(),
                    item.metric.clone(),
                    item.target.clone(),
                    item.source.clone(),
                ),
                item,
            )
        })
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(node, metric, target, source)| {
            let key = (node.clone(), metric.clone(), target.clone(), source.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            HostDiff {
                node,
                metric,
                target,
                source,
                presence: presence(&base, &candidate),
                base_unit: base.as_ref().map(|item| item.unit.clone()),
                candidate_unit: candidate.as_ref().map(|item| item.unit.clone()),
                average: NumericDiff::new(
                    base.as_ref().map(|item| item.average),
                    candidate.as_ref().map(|item| item.average),
                ),
                peak: NumericDiff::new(
                    base.as_ref().map(|item| item.peak),
                    candidate.as_ref().map(|item| item.peak),
                ),
                base_peak_at: base
                    .as_ref()
                    .and_then(|item| item.peak_at.map(|value| value.to_rfc3339())),
                candidate_peak_at: candidate
                    .as_ref()
                    .and_then(|item| item.peak_at.map(|value| value.to_rfc3339())),
            }
        })
        .collect();
    compact(items, |a, b| {
        b.peak
            .magnitude()
            .total_cmp(&a.peak.magnitude())
            .then_with(|| b.average.magnitude().total_cmp(&a.average.magnitude()))
            .then_with(|| a.metric.cmp(&b.metric))
    })
}

fn transition_diff(
    base: Vec<Transition>,
    candidate: Vec<Transition>,
) -> DiffSection<TransitionDiff> {
    type Key = (String, String);
    let mut base = base
        .into_iter()
        .map(|item| ((item.from_route.clone(), item.to_route.clone()), item))
        .collect::<BTreeMap<Key, _>>();
    let mut candidate = candidate
        .into_iter()
        .map(|item| ((item.from_route.clone(), item.to_route.clone()), item))
        .collect::<BTreeMap<Key, _>>();
    let keys = base
        .keys()
        .chain(candidate.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let items = keys
        .into_iter()
        .map(|(from_route, to_route)| {
            let key = (from_route.clone(), to_route.clone());
            let base = base.remove(&key);
            let candidate = candidate.remove(&key);
            TransitionDiff {
                from_route,
                to_route,
                presence: presence(&base, &candidate),
                count: NumericDiff::new(
                    base.as_ref().map(|item| item.count as f64),
                    candidate.as_ref().map(|item| item.count as f64),
                ),
                p50_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p50_ms),
                    candidate.as_ref().and_then(|item| item.p50_ms),
                ),
                p95_ms: NumericDiff::new(
                    base.as_ref().and_then(|item| item.p95_ms),
                    candidate.as_ref().and_then(|item| item.p95_ms),
                ),
            }
        })
        .collect();
    compact(items, |a, b| {
        b.count
            .magnitude()
            .total_cmp(&a.count.magnitude())
            .then_with(|| a.from_route.cmp(&b.from_route))
            .then_with(|| a.to_route.cmp(&b.to_route))
    })
}

pub fn write_json(diff: &RunDiff, writer: impl Write) -> Result<()> {
    serde_json::to_writer_pretty(writer, diff)?;
    Ok(())
}

pub fn write_html(diff: &RunDiff, mut writer: impl Write) -> Result<()> {
    let coverage = diff
        .coverage
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} → {}</td></tr>",
                item.presence.as_str(),
                escape(&item.section),
                escape(&item.node),
                escape(&item.collector),
                escape(&item.phase),
                escape(item.base_status.as_deref().unwrap_or("-")),
                escape(item.candidate_status.as_deref().unwrap_or("-")),
            )
        })
        .collect::<String>();
    let http = diff
        .http
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                item.presence.as_str(), escape(&item.node), escape(&item.method), escape(&item.route),
                numeric(&item.total_ms), numeric(&item.count), numeric(&item.p95_ms),
            )
        })
        .collect::<String>();
    let database = diff
        .database
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                item.presence.as_str(),
                escape(&item.node),
                escape(&item.engine),
                escape(&item.digest),
                numeric(&item.total_ms),
                numeric(&item.calls),
            )
        })
        .collect::<String>();
    let cpu = diff
        .cpu
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                item.presence.as_str(),
                escape(&item.node),
                escape(&item.process),
                escape(&item.symbol),
                numeric(&item.sample_percent),
            )
        })
        .collect::<String>();
    let host = diff
        .host
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                item.presence.as_str(),
                escape(&item.node),
                escape(&item.metric),
                escape(&item.target),
                numeric(&item.average),
                numeric(&item.peak),
            )
        })
        .collect::<String>();
    let transitions = diff
        .transitions
        .items
        .iter()
        .map(|item| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                item.presence.as_str(),
                escape(&item.from_route),
                escape(&item.to_route),
                numeric(&item.count),
                numeric(&item.p95_ms),
            )
        })
        .collect::<String>();
    let embedded = serde_json::to_string(diff)?.replace('<', "\\u003c");
    let title = format!(
        "isuscope diff {} → {}",
        short(&diff.base.id),
        short(&diff.candidate.id)
    );
    write!(
        writer,
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>
:root{{color-scheme:dark;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#0b0f14;color:#d8dee9}}body{{margin:0}}main{{max-width:1500px;margin:auto;padding:24px}}a{{color:#88c0d0}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}}.card,section{{background:#121923;border:1px solid #263241;border-radius:10px;padding:16px;margin:14px 0}}.label{{color:#8c9bab;font-size:12px}}.value{{font-size:24px;font-weight:700}}table{{width:100%;border-collapse:collapse;font-size:12px}}th,td{{text-align:left;padding:7px;border-bottom:1px solid #263241;vertical-align:top}}th{{color:#8c9bab}}code{{white-space:pre-wrap}}</style></head><body><main><p><a href="/diff">choose runs</a> · <a href="/">latest report</a></p><h1>{title}</h1><div class="grid"><div class="card"><div class="label">BASE SCORE</div><div class="value">{base_score}</div></div><div class="card"><div class="label">CANDIDATE SCORE</div><div class="value">{candidate_score}</div></div><div class="card"><div class="label">DELTA</div><div class="value">{score_delta}</div></div><div class="card"><div class="label">DELTA %</div><div class="value">{score_percent}</div></div></div><section><h2>Coverage</h2><table><thead><tr><th>PRESENCE</th><th>SECTION</th><th>NODE</th><th>COLLECTOR</th><th>PHASE</th><th>STATUS</th></tr></thead><tbody>{coverage}</tbody></table></section><section><h2>HTTP</h2><table><thead><tr><th>PRESENCE</th><th>NODE</th><th>METHOD</th><th>ROUTE</th><th>TOTAL ms</th><th>COUNT</th><th>P95 ms</th></tr></thead><tbody>{http}</tbody></table></section><section><h2>Database</h2><table><thead><tr><th>PRESENCE</th><th>NODE</th><th>ENGINE</th><th>DIGEST</th><th>TOTAL ms</th><th>CALLS</th></tr></thead><tbody>{database}</tbody></table></section><section><h2>CPU symbols</h2><table><thead><tr><th>PRESENCE</th><th>NODE</th><th>PROCESS</th><th>SYMBOL</th><th>SAMPLE %</th></tr></thead><tbody>{cpu}</tbody></table></section><section><h2>Host</h2><table><thead><tr><th>PRESENCE</th><th>NODE</th><th>METRIC</th><th>TARGET</th><th>AVERAGE</th><th>PEAK</th></tr></thead><tbody>{host}</tbody></table></section><section><h2>Transitions</h2><table><thead><tr><th>PRESENCE</th><th>FROM</th><th>TO</th><th>COUNT</th><th>P95 ms</th></tr></thead><tbody>{transitions}</tbody></table></section><script type="application/json" id="isuscope-diff">{embedded}</script></main></body></html>"#,
        title = escape(&title),
        base_score = integer(diff.score.base),
        candidate_score = integer(diff.score.candidate),
        score_delta = integer(diff.score.delta),
        score_percent = diff
            .score
            .delta_percent
            .map_or_else(|| "-".into(), |value| format!("{value:+.2}%")),
    )?;
    Ok(())
}

fn numeric(value: &NumericDiff) -> String {
    format!(
        "{} → {} ({})",
        decimal(value.base),
        decimal(value.candidate),
        value
            .delta
            .map_or_else(|| "-".into(), |delta| format!("{delta:+.2}"))
    )
}

fn decimal(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |value| format!("{value:.2}"))
}

fn integer(value: Option<i64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: String, total_ms: f64) -> HttpRouteSummary {
        HttpRouteSummary {
            node: "web-1".into(),
            method: "GET".into(),
            route: name,
            count: 1.0,
            total_ms: Some(total_ms),
            ..Default::default()
        }
    }

    #[test]
    fn diff_joins_full_inputs_before_compacting() {
        let mut base = (0..20)
            .map(|index| route(format!("/stable-{index:02}"), 1000.0 - index as f64))
            .collect::<Vec<_>>();
        base.push(route("/was-rank-21".into(), 1.0));
        let mut candidate = (0..20)
            .map(|index| route(format!("/stable-{index:02}"), 1000.0 - index as f64))
            .collect::<Vec<_>>();
        candidate.push(route("/was-rank-21".into(), 2001.0));

        let section = http_diff(base, candidate);

        assert_eq!(section.total_count, 21);
        assert!(section.truncated);
        assert_eq!(section.items.len(), COMPACT_LIMIT);
        let changed = section
            .items
            .iter()
            .find(|item| item.route == "/was-rank-21")
            .expect("a large delta from outside the original top 20 must remain visible");
        assert_eq!(changed.presence, Presence::Both);
        assert_eq!(changed.total_ms.base, Some(1.0));
        assert_eq!(changed.total_ms.candidate, Some(2001.0));
        assert_eq!(changed.total_ms.delta, Some(2000.0));
    }

    #[test]
    fn zero_base_has_no_percentage_delta() {
        let values = NumericDiff::new(Some(0.0), Some(10.0));
        assert_eq!(values.delta, Some(10.0));
        assert_eq!(values.delta_percent, None);

        let score = score_diff(Some(0), Some(10));
        assert_eq!(score.delta, Some(10));
        assert_eq!(score.delta_percent, None);
    }

    #[test]
    fn union_marks_added_and_removed_routes() {
        let section = http_diff(
            vec![route("/removed".into(), 10.0)],
            vec![route("/added".into(), 20.0)],
        );
        assert_eq!(section.total_count, 2);
        assert_eq!(
            section
                .items
                .iter()
                .find(|item| item.route == "/added")
                .unwrap()
                .presence,
            Presence::Added
        );
        assert_eq!(
            section
                .items
                .iter()
                .find(|item| item.route == "/removed")
                .unwrap()
                .presence,
            Presence::Removed
        );
    }
}
