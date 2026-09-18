use super::*;

/// 利用者とAIが最初に読む文書。`isuscope init`が各projectへ配るSETUP.mdも含める。
const DOCUMENTS: [&str; 3] = [
    "README.md",
    "templates/SETUP.md",
    "docs/standard-observability.md",
];

/// 廃止したcommand。文書では「廃止した」と説明する行でだけ触れてよい。
const REMOVED_COMMANDS: [&str; 3] = ["report", "diff", "metrics"];

fn read(document: &str) -> String {
    fs::read_to_string(format!("{}/{document}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// 文書の例に、今のCLIが受け付けないコマンドが残っていないことを確かめる。
/// 手順書の乗り換え先を間違えると、当日そこで止まる。
#[test]
fn every_documented_command_is_accepted_by_the_cli() {
    let mut checked = Vec::new();
    for document in DOCUMENTS {
        for invocation in documented_invocations(&read(document)) {
            if checked.contains(&invocation) {
                continue;
            }
            let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
                .args(&invocation)
                .arg("--help")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{document} documents `isuscope {}`, which the CLI rejects:\n{}",
                invocation.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            checked.push(invocation);
        }
    }
    assert!(checked.len() >= 8, "found only {} examples", checked.len());
}

/// 廃止したcommandを、使えるcommandとして案内していないこと。
#[test]
fn removed_commands_are_mentioned_only_as_removed() {
    for document in DOCUMENTS {
        for (number, line) in read(document).lines().enumerate() {
            for command in REMOVED_COMMANDS {
                if line.contains(&format!("`{command}`")) && !line.contains("廃止") {
                    panic!(
                        "{document}:{} presents the removed command `{command}`:\n{line}",
                        number + 1
                    );
                }
            }
        }
    }
}

/// `--base`へ渡す例は、placeholderか`latest`にする。`previous`のような語は受け付けない。
#[test]
fn documented_run_selectors_resolve() {
    for document in DOCUMENTS {
        for (number, line) in read(document).lines().enumerate() {
            let words = line.split_whitespace().collect::<Vec<_>>();
            for pair in words.windows(2) {
                if pair[0] != "--base" {
                    continue;
                }
                let value = pair[1].trim_matches(|character| matches!(character, '`' | '"'));
                let placeholder = value
                    .chars()
                    .all(|character| character.is_ascii_uppercase() || character == '_');
                assert!(
                    placeholder || value == "latest",
                    "{document}:{} passes `--base {value}`, which is not a run selector",
                    number + 1
                );
            }
        }
    }
}

/// `isuscope`に続く部分から、subcommandとして解釈できる語だけを取り出す。
/// 引数やplaceholder（`--flag`、`RUN_ID`、`<path>`、`"..."`）で打ち切る。
fn documented_invocations(document: &str) -> Vec<Vec<String>> {
    let mut invocations = Vec::new();
    for line in document.lines() {
        // 見出しとshellのcommentはcommandではない。
        if line.trim_start().starts_with('#') {
            continue;
        }
        for (index, _) in line.match_indices("isuscope ") {
            let preceded_by = line[..index].chars().last();
            if !matches!(preceded_by, None | Some(' ') | Some('`') | Some('$')) {
                continue;
            }
            let mut words = Vec::new();
            for word in line[index + "isuscope ".len()..].split_whitespace() {
                if !word
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '-')
                    || word.starts_with('-')
                {
                    break;
                }
                words.push(word.to_owned());
            }
            if !words.is_empty() {
                invocations.push(words);
            }
        }
    }
    invocations
}
