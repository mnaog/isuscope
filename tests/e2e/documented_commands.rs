use super::*;

/// READMEの例に、今のCLIが受け付けないコマンドが残っていないことを確かめる。
/// 手順書の乗り換え先を間違えると、当日そこで止まる。
#[test]
fn every_command_in_the_readme_is_accepted_by_the_cli() {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    let mut checked = Vec::new();
    for invocation in documented_invocations(&readme) {
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
            "README documents `isuscope {}`, which the CLI rejects:\n{}",
            invocation.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        checked.push(invocation);
    }
    assert!(checked.len() >= 8, "found only {} examples", checked.len());
}

/// `isuscope`に続く部分から、subcommandとして解釈できる語だけを取り出す。
/// 引数やplaceholder（`--flag`、`RUN_ID`、`<path>`、`"..."`）で打ち切る。
fn documented_invocations(document: &str) -> Vec<Vec<String>> {
    let mut invocations = Vec::new();
    for line in document.lines() {
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
