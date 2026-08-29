use crate::{
    model::Transition,
    query::{self, MetricQueryOutput, MetricQueryRow},
    report::{
        CoverageSummary, CpuSummary, DatabaseSummary, HostSummary, HttpRouteSummary,
        ProfileArtifact, RunDiagnostics,
    },
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct BriefOutput {
    pub schema_version: u32,
    pub run: BriefRun,
    pub coverage_issues: BriefSection<CoverageIssueGroup>,
    pub coverage_info_count: usize,
    pub benchmark: BriefSection<MetricQueryRow>,
    pub http: BriefSection<HttpRouteSummary>,
    pub database: BriefSection<DatabaseSummary>,
    pub omitted_alternative_database_rows: usize,
    pub cpu: BriefSection<CpuSummary>,
    pub host: BriefSection<HostSummary>,
    pub transitions: BriefSection<Transition>,
    pub artifact_issues: BriefSection<ProfileArtifact>,
    pub unavailable_artifact_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BriefRun {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: String,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    pub hypothesis: String,
    pub analysis_status: String,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub metric_count: usize,
}

#[derive(Debug, Serialize)]
pub struct BriefSection<T> {
    pub total_count: usize,
    pub truncated: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Serialize)]
pub struct CoverageIssueGroup {
    pub severity: &'static str,
    pub section: String,
    pub collector: String,
    pub phase: String,
    pub status: String,
    pub nodes: Vec<String>,
    pub missing_metrics: Vec<String>,
    pub occurrences: usize,
}

pub fn build(
    diagnostics: RunDiagnostics,
    benchmark: MetricQueryOutput,
    limit: usize,
) -> BriefOutput {
    let run = diagnostics.run;
    let (coverage_issues, coverage_info_count) = coverage_issues(diagnostics.coverage);
    let mut http = diagnostics.http;
    http.iter_mut().for_each(query::round_http_summary);
    let (mut database, omitted_alternative_database_rows) =
        preferred_database(diagnostics.database);
    database.iter_mut().for_each(query::round_database_summary);
    let mut host = diagnostics.host;
    host.iter_mut().for_each(|row| {
        row.average = query::round_to(row.average, 3);
        row.peak = query::round_to(row.peak, 3);
    });
    let unavailable_artifact_count = diagnostics
        .artifacts
        .iter()
        .filter(|item| item.status == "unavailable")
        .count();
    BriefOutput {
        schema_version: 1,
        run: BriefRun {
            id: run.id,
            started_at: run.started_at.to_rfc3339(),
            finished_at: run.finished_at.map(|value| value.to_rfc3339()),
            state: run.state.as_str().into(),
            score: run.benchmark.score,
            passed: run.benchmark.passed,
            hypothesis: run.hypothesis,
            analysis_status: run.analysis_status.as_str().into(),
            commit_hash: run.source.commit_hash,
            dirty: run.source.dirty,
            metric_count: run.metric_count,
        },
        coverage_issues: section(coverage_issues, limit),
        coverage_info_count,
        benchmark: section(benchmark.rows, limit),
        http: section(http, limit),
        database: section(database, limit),
        omitted_alternative_database_rows,
        cpu: section(diagnostics.cpu, limit),
        host: section(host, limit),
        transitions: section(diagnostics.transitions, limit),
        artifact_issues: section(
            diagnostics
                .artifacts
                .into_iter()
                .filter(|item| item.status == "failed")
                .collect(),
            limit,
        ),
        unavailable_artifact_count,
        warnings: benchmark.warnings,
    }
}

fn coverage_issues(coverage: Vec<CoverageSummary>) -> (Vec<CoverageIssueGroup>, usize) {
    type Key = (String, String, String, String, Vec<String>);
    let mut groups = BTreeMap::<Key, Vec<String>>::new();
    let mut info_count = 0;
    for item in coverage {
        let severity = match item.status.as_str() {
            "failed" => "critical",
            "missing" => "warning",
            _ if item.missing_metrics.is_empty() => continue,
            _ => "info",
        };
        if severity == "info" {
            info_count += 1;
            continue;
        }
        groups
            .entry((
                item.section,
                item.collector,
                item.phase,
                item.status,
                item.missing_metrics,
            ))
            .or_default()
            .push(item.node);
    }
    let mut issues = groups
        .into_iter()
        .map(
            |((section, collector, phase, status, missing_metrics), mut nodes)| {
                nodes.sort();
                nodes.dedup();
                CoverageIssueGroup {
                    severity: if status == "failed" {
                        "critical"
                    } else {
                        "warning"
                    },
                    section,
                    collector,
                    phase,
                    status,
                    occurrences: nodes.len(),
                    nodes,
                    missing_metrics,
                }
            },
        )
        .collect::<Vec<_>>();
    issues.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| a.section.cmp(&b.section))
            .then_with(|| a.collector.cmp(&b.collector))
    });
    (issues, info_count)
}

fn severity_rank(value: &str) -> u8 {
    match value {
        "critical" => 0,
        "warning" => 1,
        _ => 2,
    }
}

fn preferred_database(database: Vec<DatabaseSummary>) -> (Vec<DatabaseSummary>, usize) {
    if !database.iter().any(|item| item.source == "mysql-log-delta") {
        return (database, 0);
    }
    let total_count = database.len();
    let database = database
        .into_iter()
        .filter(|item| item.source != "slp")
        .collect::<Vec<_>>();
    let omitted = total_count - database.len();
    (database, omitted)
}

fn section<T>(mut items: Vec<T>, limit: usize) -> BriefSection<T> {
    let total_count = items.len();
    items.truncate(limit);
    BriefSection {
        total_count,
        truncated: total_count > items.len(),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_issues_group_nodes_and_hide_info_rows() {
        let coverage = ["app2", "app3"]
            .into_iter()
            .map(|node| CoverageSummary {
                section: "http".into(),
                node: node.into(),
                collector: "alp".into(),
                phase: "after".into(),
                status: "missing".into(),
                missing_metrics: vec!["http.requests".into()],
            })
            .chain(std::iter::once(CoverageSummary {
                section: "cpu".into(),
                node: "app1".into(),
                collector: "perf".into(),
                phase: "after".into(),
                status: "unavailable".into(),
                missing_metrics: vec!["cpu.sample_count".into()],
            }))
            .collect();
        let (issues, info_count) = coverage_issues(coverage);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].nodes, ["app2", "app3"]);
        assert_eq!(issues[0].occurrences, 2);
        assert_eq!(info_count, 1);
    }
}
