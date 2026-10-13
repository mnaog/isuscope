//! 同じ構成を、意図せずもう一度測るのを止める。
//!
//! ベンチを実際に始めた直前のrunと、配ったもの（`identity_command`の出力。無ければsourceの状態）、
//! nodeのfingerprint、計測の設定（`config.toml`、`routes.toml`、`[tooling] include`のfile、
//! isuscopeの版）、runの種類を比べる。isuscopeから見える違いが1つも無ければ、`--repeat`で理由を
//! 書かない限りベンチを始めない。揃っていない値（片方にしか無いfingerprintなど）は「同じ」とは
//! 扱わず、違いとして挙げる。

use crate::model::{RepeatCheck, RunIdentity, RunManifest};
use std::collections::{BTreeMap, BTreeSet};

/// `identity_command`の出力。JSON objectなら各fieldを文字列にして持ち、比較と表示に使う。
pub fn identity(stdout: &[u8]) -> RunIdentity {
    use sha2::{Digest, Sha256};
    let values = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(stdout)
        .map(|object| {
            object
                .into_iter()
                .map(|(key, value)| {
                    let text = match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    };
                    (key, text)
                })
                .collect()
        })
        .unwrap_or_default();
    RunIdentity {
        sha256: format!("{:x}", Sha256::digest(stdout)),
        values,
    }
}

/// `base`（ベンチを始めた直前のrun）と今回の違いを並べる。fingerprintは
/// `Store::fingerprint_index`と同じ鍵で渡す。
pub fn compare(
    base: &RunManifest,
    base_fingerprints: &BTreeMap<String, String>,
    current: &RunManifest,
    current_fingerprints: &BTreeMap<String, String>,
    reason: Option<String>,
) -> RepeatCheck {
    let mut changed = Vec::new();
    if base.mode != current.mode {
        changed.push("mode".into());
    }
    match (&base.identity, &current.identity) {
        (Some(before), Some(after)) if before.values.is_empty() || after.values.is_empty() => {
            if before.sha256 != after.sha256 {
                changed.push("identity".into());
            }
        }
        (Some(before), Some(after)) => {
            let keys = before
                .values
                .keys()
                .chain(after.values.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                if before.values.get(key) != after.values.get(key) {
                    changed.push(format!("identity.{key}"));
                }
            }
        }
        (None, None) => {
            // 配ったものを報告するcommandが無ければ、sourceの状態で代える。
            if base.source.state_sha256.is_empty()
                || base.source.state_sha256 != current.source.state_sha256
            {
                changed.push("source".into());
            }
        }
        _ => changed.push("identity".into()),
    }
    let (before, after) = (&base.tooling, &current.tooling);
    if before.isuscope_version != after.isuscope_version {
        changed.push("observation.isuscope_version".into());
    }
    if before.config_sha256.is_empty() || before.config_sha256 != after.config_sha256 {
        changed.push("observation.config".into());
    }
    if before.routes_sha256 != after.routes_sha256 {
        changed.push("observation.routes".into());
    }
    let files = before
        .extra_files_sha256
        .keys()
        .chain(after.extra_files_sha256.keys())
        .collect::<BTreeSet<_>>();
    for file in files {
        if before.extra_files_sha256.get(file) != after.extra_files_sha256.get(file) {
            changed.push(format!("observation.{file}"));
        }
    }
    let keys = base_fingerprints
        .keys()
        .chain(current_fingerprints.keys())
        .collect::<BTreeSet<_>>();
    for key in keys {
        if base_fingerprints.get(key) != current_fingerprints.get(key) {
            changed.push(format!("fingerprint.{}", fingerprint_label(key)));
        }
    }
    RepeatCheck {
        base_run_id: base.id.clone(),
        changed,
        reason,
    }
}

/// `["name",{"node":"app1"}]`の鍵を`app1.name`と読める形にする。
fn fingerprint_label(key: &str) -> String {
    match serde_json::from_str::<(String, BTreeMap<String, String>)>(key) {
        Ok((name, labels)) => match labels.get("node") {
            Some(node) => format!("{node}.{name}"),
            None => name,
        },
        Err(_) => key.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(identity: Option<&str>) -> RunManifest {
        let mut manifest: RunManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 6, "id": "01a1017e-d070-71c2-a89f-0835b7c5d88e", "mode": "run",
            "state": "complete", "started_at": "2026-09-19T00:00:00Z", "finished_at": null,
            "source": {"repository": ".", "git_available": true, "commit_hash": null,
                "branch": null, "dirty": false, "state_sha256": "aaaa", "untracked": [], "error": null},
            "tooling": {"isuscope_version": "0.9.0", "config_sha256": "cccc", "routes_sha256": null,
                "setup_script_sha256": null, "setup_state_sha256": null, "error": null},
            "benchmark": {"mode": "command", "command": [], "exit_code": 0, "score": 1,
                "passed": true, "messages": [], "error": null,
                "initialize_started_at": null, "initialize_finished_at": null},
            "collectors": [], "logs": [], "metric_count": 0, "transition_count": 0
        }))
        .unwrap();
        manifest.identity = identity.map(|text| super::identity(text.as_bytes()));
        manifest
    }

    fn fingerprints(value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(
            serde_json::to_string(&("app.binary", BTreeMap::from([("node", "app1")]))).unwrap(),
            value.to_owned(),
        )])
    }

    #[test]
    fn nothing_changed_is_reported_as_an_empty_list() {
        let base = manifest(Some(r#"{"commit":"abc","deployed":true}"#));
        let current = manifest(Some(r#"{"commit":"abc","deployed":true}"#));
        let check = compare(
            &base,
            &fingerprints("x"),
            &current,
            &fingerprints("x"),
            None,
        );
        assert!(check.changed.is_empty(), "{:?}", check.changed);
    }

    #[test]
    fn identity_fields_and_fingerprints_are_named_when_they_differ() {
        let base = manifest(Some(r#"{"commit":"abc","deployed":true}"#));
        let current = manifest(Some(r#"{"commit":"def","deployed":true}"#));
        let check = compare(
            &base,
            &fingerprints("x"),
            &current,
            &fingerprints("y"),
            None,
        );
        assert_eq!(
            check.changed,
            ["identity.commit", "fingerprint.app1.app.binary"]
        );
    }

    #[test]
    fn a_missing_fingerprint_is_a_difference_not_a_match() {
        let base = manifest(None);
        let current = manifest(None);
        let check = compare(&base, &fingerprints("x"), &current, &BTreeMap::new(), None);
        assert_eq!(check.changed, ["fingerprint.app1.app.binary"]);
    }

    #[test]
    fn without_identity_the_source_state_stands_in_for_the_deployment() {
        let base = manifest(None);
        let mut current = manifest(None);
        current.source.state_sha256 = "bbbb".into();
        let check = compare(&base, &BTreeMap::new(), &current, &BTreeMap::new(), None);
        assert_eq!(check.changed, ["source"]);
    }

    #[test]
    fn plain_text_identity_is_compared_as_a_whole() {
        let base = manifest(Some("deploy 1\n"));
        let current = manifest(Some("deploy 2\n"));
        let check = compare(&base, &BTreeMap::new(), &current, &BTreeMap::new(), None);
        assert_eq!(check.changed, ["identity"]);
    }
}
