//! Anthropic: the official Python SDK's generated `types/` modules are plain text, and the only
//! machine-readable statement of the Messages API Anthropic publishes. The workflow downloads
//! `src/anthropic/types` (and `beta/`) from the SDK repository; this module reads them.

use std::collections::BTreeSet;

use airmate_core::policy::{anthropic::TABLE, Group};

use crate::{compare, merge, table_names, Drift, Error};

/// A downloaded file: its path under `types/` and its text.
pub type File = (String, String);

/// The quoted strings inside every `Literal[...]` in `text`, which may span lines.
fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("Literal[") {
        let after = &rest[i + "Literal[".len()..];
        let Some(end) = after.find(']') else { break };
        let mut inner = &after[..end];
        while let Some(q) = inner.find('"') {
            let tail = &inner[q + 1..];
            let Some(close) = tail.find('"') else { break };
            out.push(tail[..close].to_owned());
            inner = &tail[close + 1..];
        }
        rest = &after[end..];
    }
    out
}

/// The attribute names of the classes whose name starts with `prefix`, skipping docstrings (whose
/// lines can look like `Note: …`). The streaming and non-streaming variants of a request add
/// `stream`, so the prefix covers all three.
fn class_fields(text: &str, prefix: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    let mut in_doc = false;
    for line in text.lines() {
        if in_doc {
            if line.contains("\"\"\"") {
                in_doc = false;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("\"\"\"") {
            if trimmed.matches("\"\"\"").count() < 2 {
                in_doc = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("class ") {
            inside = rest
                .split(['(', ':'])
                .next()
                .is_some_and(|n| n.trim().starts_with(prefix));
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(body) = line.strip_prefix("    ") {
            if body.starts_with(' ') {
                continue;
            }
            if let Some((name, _)) = body.split_once(':') {
                if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.is_empty()
                    && !name.starts_with('_')
                {
                    out.insert(name.to_owned());
                }
            }
        }
    }
    out
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The member class names of the first `Union[...]` after `alias` in `text`.
fn union_members(text: &str, alias: &str) -> Vec<String> {
    let Some(at) = text.find(&format!("{alias}: TypeAlias")) else {
        return Vec::new();
    };
    let after = &text[at..];
    let Some(open) = after.find("Union[") else {
        return Vec::new();
    };
    let body = &after[open + "Union[".len()..];
    let Some(close) = body.find(']') else {
        return Vec::new();
    };
    body[..close]
        .split(',')
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect()
}

/// The `type` literal declared by class `name`, wherever it is defined.
fn class_type(files: &[File], name: &str) -> Option<String> {
    let header = format!("class {name}(");
    for (_, text) in files {
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if !line.starts_with(&header) {
                continue;
            }
            for body in lines.by_ref() {
                if body.starts_with("class ") || body.starts_with("def ") {
                    break;
                }
                if body.trim_start().starts_with("type:") {
                    return literals(body).into_iter().next();
                }
            }
            return None;
        }
    }
    None
}

/// The content block types a request can carry: the members of each `ContentBlockParam` union.
/// Reading the union, rather than every `*_block_param.py` file, keeps out the nested result
/// objects those files also define.
fn block_types(files: &[File]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (path, text) in files
        .iter()
        .filter(|(p, _)| base(p).ends_with("content_block_param.py"))
    {
        let _ = path;
        for member in union_members(text, "ContentBlockParam")
            .into_iter()
            .chain(union_members(text, "BetaContentBlockParam"))
        {
            out.extend(class_type(files, &member));
        }
    }
    out
}

/// `type: Literal["x"]` values (with or without `Required[...]`) in the files `select` picks.
fn type_literals(files: &[File], select: impl Fn(&str) -> bool) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (path, text) in files.iter().filter(|(p, _)| select(base(p))) {
        let _ = path;
        for line in text.lines() {
            if line.trim_start().starts_with("type:") {
                out.extend(literals(line));
            }
        }
    }
    out
}

/// What the SDK defines for one policy table group: the group, a name for reports, and the names.
type Found = (Group, &'static str, BTreeSet<String>);

/// Request fields, content block types, thinking types, stop reasons and stream delta types in the
/// SDK files.
fn find(files: &[File]) -> Result<[Found; 5], Error> {
    let mut request = BTreeSet::new();
    for (path, text) in files
        .iter()
        .filter(|(p, _)| base(p).ends_with("message_create_params.py"))
    {
        let _ = path;
        request.extend(class_fields(text, "MessageCreateParams"));
    }
    // The SDK takes `betas` as a parameter but sends it as the `anthropic-beta` header.
    request.remove("betas");
    let blocks = block_types(files);
    let thinking = type_literals(files, |b| {
        b.contains("thinking_config_") && b.ends_with("_param.py")
    });
    let mut stops = BTreeSet::new();
    for (_, text) in files
        .iter()
        .filter(|(p, _)| base(p).ends_with("stop_reason.py"))
    {
        stops.extend(literals(text));
    }
    let deltas = type_literals(files, |b| b.ends_with("_delta.py") && !b.contains("event"));
    // A layout this parser does not understand finds (almost) nothing; say so instead of reporting
    // "no drift".
    for (what, got, min) in [
        ("request fields", request.len(), 10),
        ("content block types", blocks.len(), 5),
        ("thinking types", thinking.len(), 2),
        ("stop reasons", stops.len(), 3),
        ("stream delta types", deltas.len(), 3),
    ] {
        if got < min {
            return Err(Error(format!(
                "found only {got} {what} in the SDK files (expected at least {min}): the SDK layout has probably changed and this parser needs updating"
            )));
        }
    }
    Ok([
        (Group::Request, "request field", request),
        (Group::ContentBlock, "content block type", blocks),
        (Group::ThinkingType, "thinking type", thinking),
        (Group::StopReason, "stop reason", stops),
        (Group::StreamDelta, "stream delta type", deltas),
    ])
}

/// Compares the SDK files with the Anthropic policy table.
///
/// # Errors
///
/// The files are not in a layout this parser understands.
pub fn analyze(files: &[File]) -> Result<Drift, Error> {
    let found = find(files)?;
    Ok(merge(
        found
            .into_iter()
            .map(|(group, what, names)| compare(what, &names, &table_names(TABLE, group)))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_read_from_unions_and_required_wrappers() {
        assert_eq!(literals(r#"    type: Required[Literal["text"]]"#), ["text"]);
        assert_eq!(
            literals(r#"StopReason: TypeAlias = Literal["end_turn", "max_tokens", "tool_use"]"#),
            ["end_turn", "max_tokens", "tool_use"]
        );
        assert_eq!(literals("    type: str"), Vec::<String>::new());
        assert_eq!(literals(r#"x: Literal["a"] | Literal["b"]"#), ["a", "b"]);
        assert_eq!(literals("StopReason: TypeAlias = Literal[\n    \"end_turn\", \"max_tokens\", \"tool_use\"\n]\n"), ["end_turn", "max_tokens", "tool_use"], "a union may span lines");
    }

    #[test]
    fn class_fields_skip_docstrings_and_other_classes() {
        let text = r#"class Other(TypedDict, total=False):
    ignored: int

class MessageCreateParamsBase(TypedDict, total=False):
    max_tokens: Required[int]
    """The maximum number of tokens.

    Note: this text looks like a field.
    Example: so does this.
    """

    messages: Required[Iterable[MessageParam]]
    """One-line docstring."""

    metadata: MetadataParam

    _private: int

class MessageCreateParamsNonStreaming(MessageCreateParamsBase, total=False):
    stream: Literal[False]
"#;
        let f = class_fields(text, "MessageCreateParams");
        assert_eq!(
            f.into_iter().collect::<Vec<_>>(),
            ["max_tokens", "messages", "metadata", "stream"],
            "the streaming variant's `stream` counts too"
        );
    }

    fn file(path: &str, text: &str) -> File {
        (path.to_owned(), text.to_owned())
    }

    fn fixture() -> Vec<File> {
        let fields: String = [
            "model",
            "max_tokens",
            "messages",
            "system",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "tools",
            "tool_choice",
            "thinking",
            "metadata",
            "stop_sequences",
            "service_tier",
            "output_config",
            "brand_new_field",
        ]
        .iter()
        .map(|f| format!("    {f}: Required[object]\n    \"\"\"doc\"\"\"\n\n"))
        .collect();
        let mut files = vec![file(
            "message_create_params.py",
            &format!("class MessageCreateParamsBase(TypedDict, total=False):\n{fields}"),
        )];
        let blocks = [
            "text",
            "image",
            "document",
            "thinking",
            "tool_use",
            "tool_result",
            "search_result",
            "new_block",
        ];
        let class = |t: &str| {
            t.split('_')
                .map(|w| format!("{}{}", w[..1].to_uppercase(), &w[1..]))
                .collect::<String>()
                + "BlockParam"
        };
        let members: String = blocks
            .iter()
            .map(|t| format!("    {},\n", class(t)))
            .collect();
        files.push(file(
            "content_block_param.py",
            &format!("ContentBlockParam: TypeAlias = Union[\n{members}]\n"),
        ));
        for t in blocks {
            files.push(file(
                &format!("{t}_block_param.py"),
                &format!("class {}(TypedDict, total=False):\n    type: Required[Literal[\"{t}\"]]\n\nclass Nested{}Result(TypedDict):\n    type: Required[Literal[\"nested_{t}_result\"]]\n", class(t), class(t)),
            ));
        }
        for t in ["enabled", "disabled", "adaptive"] {
            files.push(file(
                &format!("thinking_config_{t}_param.py"),
                &format!("class X(TypedDict):\n    type: Required[Literal[\"{t}\"]]\n"),
            ));
        }
        files.push(file("stop_reason.py", "StopReason: TypeAlias = Literal[\"end_turn\", \"max_tokens\", \"stop_sequence\", \"tool_use\", \"pause_turn\", \"refusal\"]\n"));
        for t in [
            "text_delta",
            "input_json_delta",
            "thinking_delta",
            "signature_delta",
            "citations_delta",
        ] {
            files.push(file(
                &format!("{t}.py").replace("_delta.py", "_delta.py"),
                &format!("class X(BaseModel):\n    type: Literal[\"{t}\"]\n"),
            ));
        }
        files
    }

    #[test]
    fn new_fields_and_block_types_are_unclassed_drift() {
        let d = analyze(&fixture()).unwrap();
        assert!(
            d.unclassed
                .contains(&("request field".to_owned(), "brand_new_field".to_owned())),
            "{d:?}"
        );
        assert!(
            d.unclassed
                .contains(&("content block type".to_owned(), "new_block".to_owned())),
            "{d:?}"
        );
        assert!(d.is_drift());
        // Things the table knows and the fixture lacks are informational only.
        assert!(d
            .gone
            .iter()
            .any(|(w, n)| w == "request field" && n == "container"));
    }

    #[test]
    fn nested_result_objects_are_not_content_block_types() {
        let d = analyze(&fixture()).unwrap();
        assert!(
            !d.unclassed.iter().any(|(_, n)| n.starts_with("nested_")),
            "{d:?}"
        );
    }

    #[test]
    fn unions_and_class_types_are_read() {
        let text = "ContentBlockParam: TypeAlias = Union[\n    A,\n    B,\n]\n\nOther = 1\n";
        assert_eq!(union_members(text, "ContentBlockParam"), ["A", "B"]);
        assert!(union_members(text, "Missing").is_empty());
        let files = vec![("a.py".to_owned(), "class A(TypedDict):\n    x: int\n    type: Required[Literal[\"a\"]]\n\nclass B(TypedDict):\n    y: int\n".to_owned())];
        assert_eq!(class_type(&files, "A").as_deref(), Some("a"));
        assert_eq!(
            class_type(&files, "B"),
            None,
            "a class with no type literal has none, and does not borrow the next one's"
        );
    }

    #[test]
    fn a_layout_the_parser_does_not_understand_is_an_error_not_no_drift() {
        let e = analyze(&[file(
            "message_create_params.py",
            "class Renamed(TypedDict):\n    a: int\n",
        )])
        .unwrap_err();
        assert!(e.0.contains("layout has probably changed"), "{e}");
        assert!(analyze(&[]).is_err());
    }

    #[test]
    fn beta_files_are_read_alongside_the_ga_ones() {
        let mut files = fixture();
        files.push(file("beta/message_create_params.py", "class MessageCreateParamsBase(TypedDict, total=False):\n    betas: List[str]\n    mcp_servers: Iterable[object]\n    new_beta_option: str\n"));
        let d = analyze(&files).unwrap();
        assert!(d
            .unclassed
            .contains(&("request field".to_owned(), "new_beta_option".to_owned())));
        assert!(
            !d.unclassed.iter().any(|(_, n)| n == "betas"),
            "`betas` is sent as a header, not in the body"
        );
        assert!(
            !d.unclassed
                .contains(&("request field".to_owned(), "mcp_servers".to_owned())),
            "mcp_servers is classed (rejected)"
        );
    }
}
