//! The generated policy tables in `docs/translation.md` match the tables the code uses.
//!
//! `UPDATE_DOCS=1 cargo test --test docs` rewrites the blocks; review the diff.

use airmate_core::ir::Protocol;
use airmate_core::policy::{render_markdown, table};

const DOC: &str = "docs/translation.md";

fn blocks() -> [(&'static str, Protocol); 4] {
    [
        ("anthropic", Protocol::Anthropic),
        ("chat", Protocol::Chat),
        ("responses", Protocol::Responses),
        ("gemini", Protocol::Gemini),
    ]
}

fn rewrite(doc: &str, name: &str, body: &str) -> String {
    let begin = format!("<!-- BEGIN policy:{name} -->");
    let end = format!("<!-- END policy:{name} -->");
    let i = doc
        .find(&begin)
        .unwrap_or_else(|| panic!("{DOC} has no `{begin}`"))
        + begin.len();
    let j = doc[i..]
        .find(&end)
        .unwrap_or_else(|| panic!("{DOC} has no `{end}`"))
        + i;
    format!("{}\n\n{}\n{}", &doc[..i], body.trim_end(), &doc[j..])
}

#[test]
fn the_policy_tables_in_the_translation_reference_are_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC);
    let doc = std::fs::read_to_string(&path).expect("docs/translation.md exists");
    let mut updated = doc.clone();
    for (name, protocol) in blocks() {
        updated = rewrite(&updated, name, &render_markdown(table(protocol)));
    }
    if std::env::var_os("UPDATE_DOCS").is_some() {
        std::fs::write(&path, &updated).unwrap();
        return;
    }
    assert_eq!(
        doc, updated,
        "{DOC} is out of date: run `UPDATE_DOCS=1 cargo test --test docs` and review the diff"
    );
}

#[test]
fn every_table_row_is_documented_with_a_class_and_a_reason() {
    for (name, protocol) in blocks() {
        let rows = table(protocol);
        assert!(rows.len() > 20, "{name} has only {} rows", rows.len());
        let mut seen = std::collections::HashSet::new();
        for e in rows {
            assert!(
                !e.handling.trim().is_empty(),
                "{name}: `{}` has no explanation",
                e.name
            );
            assert!(
                seen.insert((e.group, e.name)),
                "{name}: `{}` is classed twice",
                e.name
            );
        }
    }
}

// ---- the operator guide's complete examples are valid configuration ---------------------------

use std::sync::Arc;

use airmate::config::load_str;
use airmate::secret::{Secret, SystemResolver};
use airmate::server::Tables;

/// The YAML blocks in `text` whose first line is `# validate`.
fn validated_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() == "```yaml" {
            let mut block = Vec::new();
            for l in lines.by_ref() {
                if l.trim_end() == "```" {
                    break;
                }
                block.push(l);
            }
            if block.first().is_some_and(|l| l.trim() == "# validate") {
                out.push(block.join("\n") + "\n");
            }
        }
    }
    out
}

#[test]
fn every_complete_example_in_the_operator_guide_loads_and_builds() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/operator-guide.md");
    let text = std::fs::read_to_string(path).expect("docs/operator-guide.md exists");
    let blocks = validated_blocks(&text);
    assert!(
        blocks.len() >= 10,
        "found only {} validated examples",
        blocks.len()
    );
    // Every secret is given a value, so the examples build as an operator's would.
    let env = |n: &str| Some(format!("dummy-{n}"));
    for yaml in &blocks {
        let cfg =
            load_str(yaml, None).unwrap_or_else(|e| panic!("does not load:\n{e}\n---\n{yaml}"));
        Tables::build(Arc::new(cfg), &SystemResolver::new(&env), &Secret::new("t"))
            .unwrap_or_else(|e| panic!("does not build: {e:?}\n---\n{yaml}"));
    }
}

#[test]
fn the_block_finder_only_takes_marked_yaml() {
    let text =
        "```yaml\na: 1\n```\n\n```yaml\n# validate\nb: 2\n```\n\n```bash\n# validate\nc\n```\n";
    assert_eq!(
        validated_blocks(text),
        vec!["# validate\nb: 2\n".to_owned()]
    );
}
