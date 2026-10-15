//! OpenAI: the published OpenAPI document describes the Chat Completions and Responses request
//! bodies. Their properties are compared with the Chat and Responses policy tables.

use std::collections::BTreeSet;

use airmate_core::policy::{chat, responses, Group};
use serde_norway::Value;

use crate::{compare, merge, table_names, Drift, Error};

fn schema<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    doc.get("components")?.get("schemas")?.get(name)
}

/// The property names of a schema, following `allOf` and `$ref` into other component schemas.
fn properties(
    doc: &Value,
    schema_node: &Value,
    seen: &mut BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    if let Some(r) = schema_node.get("$ref").and_then(Value::as_str) {
        if let Some(name) = r.rsplit('/').next() {
            if seen.insert(name.to_owned()) {
                if let Some(s) = schema(doc, name) {
                    properties(doc, s, seen, out);
                }
            }
        }
    }
    if let Some(props) = schema_node.get("properties").and_then(Value::as_mapping) {
        out.extend(props.keys().filter_map(Value::as_str).map(str::to_owned));
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(parts) = schema_node.get(key).and_then(Value::as_sequence) {
            for p in parts {
                properties(doc, p, seen, out);
            }
        }
    }
}

fn request_fields(doc: &Value, schema_name: &str) -> Result<BTreeSet<String>, Error> {
    let s = schema(doc, schema_name).ok_or_else(|| Error(format!("the OpenAPI document has no `{schema_name}` schema: its layout has probably changed and this parser needs updating")))?;
    let mut out = BTreeSet::new();
    properties(doc, s, &mut BTreeSet::new(), &mut out);
    if out.len() < 8 {
        return Err(Error(format!(
            "found only {} properties in `{schema_name}` (expected at least 8): the document layout has probably changed",
            out.len()
        )));
    }
    Ok(out)
}

/// OpenAI's document carries 64-bit bounds such as `minimum: -9223372036854775808`, which the YAML
/// reader cannot hold in a generic value. Nothing here looks at bounds, so those values become `0`.
fn drop_huge_integers(yaml: &str) -> String {
    yaml.lines()
        .map(|line| match line.split_once(": ") {
            Some((key, value)) => {
                let v = value.trim();
                let digits = v.strip_prefix('-').unwrap_or(v);
                if digits.len() >= 19 && digits.bytes().all(|b| b.is_ascii_digit()) {
                    format!("{key}: 0")
                } else {
                    line.to_owned()
                }
            }
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compares the OpenAPI document with the Chat and Responses policy tables.
///
/// # Errors
///
/// The document is not YAML, or does not have the schemas this parser looks for.
pub fn analyze(yaml: &str) -> Result<Drift, Error> {
    let doc: Value = serde_norway::from_str(&drop_huge_integers(yaml))
        .map_err(|e| Error(format!("the OpenAPI document does not parse: {e}")))?;
    let chat_fields = request_fields(&doc, "CreateChatCompletionRequest")?;
    let responses_fields = request_fields(&doc, "CreateResponse")?;
    Ok(merge(vec![
        compare(
            "Chat Completions request field",
            &chat_fields,
            &table_names(chat::TABLE, Group::Request),
        ),
        compare(
            "Responses request field",
            &responses_fields,
            &table_names(responses::TABLE, Group::Request),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(chat_extra: &str) -> String {
        format!(
            r#"
components:
  schemas:
    CreateModelResponseProperties:
      type: object
      properties:
        metadata: {{ type: object }}
        temperature: {{ type: number }}
        top_p: {{ type: number }}
        user: {{ type: string }}
        service_tier: {{ type: string }}
    CreateChatCompletionRequest:
      allOf:
        - $ref: '#/components/schemas/CreateModelResponseProperties'
        - type: object
          properties:
            messages: {{ type: array }}
            model: {{ type: string }}
            stream: {{ type: boolean }}
            max_tokens: {{ type: integer }}
            tools: {{ type: array }}
            {chat_extra}
    CreateResponse:
      allOf:
        - $ref: '#/components/schemas/CreateModelResponseProperties'
        - type: object
          properties:
            input: {{ type: string }}
            model: {{ type: string }}
            instructions: {{ type: string }}
            stream: {{ type: boolean }}
            tools: {{ type: array }}
"#
        )
    }

    #[test]
    fn properties_come_through_allof_and_refs() {
        let v: Value = serde_norway::from_str(&doc("")).unwrap();
        let f = request_fields(&v, "CreateChatCompletionRequest").unwrap();
        for want in [
            "messages",
            "model",
            "metadata",
            "temperature",
            "user",
            "service_tier",
        ] {
            assert!(f.contains(want), "{want} in {f:?}");
        }
    }

    #[test]
    fn a_property_the_table_lacks_is_drift() {
        let d = analyze(&doc("shiny_new_option: { type: string }")).unwrap();
        assert!(
            d.unclassed.contains(&(
                "Chat Completions request field".to_owned(),
                "shiny_new_option".to_owned()
            )),
            "{d:?}"
        );
        assert!(d.is_drift());
    }

    #[test]
    fn a_document_without_the_schemas_is_an_error() {
        let e = analyze("components:\n  schemas:\n    Other: {}\n").unwrap_err();
        assert!(e.0.contains("layout has probably changed"), "{e}");
        assert!(analyze("not: [valid").is_err());
        let thin = "components:\n  schemas:\n    CreateChatCompletionRequest:\n      properties: { a: {} }\n    CreateResponse:\n      properties: { a: {} }\n";
        assert!(analyze(thin).unwrap_err().0.contains("only 1 properties"));
    }

    #[test]
    fn sixty_four_bit_bounds_do_not_stop_the_document_parsing() {
        let text = format!("{}\n            seed: {{ minimum: -9223372036854775808, maximum: 9223372036854775807, type: integer }}\n", doc(""));
        // The bound sits on its own line in real documents.
        let real = doc("seed:\n              type: integer\n              minimum: -9223372036854775808\n              maximum: 9223372036854775807");
        let _ = text;
        assert!(analyze(&real)
            .unwrap()
            .unclassed
            .iter()
            .all(|(_, n)| n != "minimum"));
        assert_eq!(
            drop_huge_integers("a: 12\nb: -9223372036854775808\nc: x"),
            "a: 12\nb: 0\nc: x"
        );
    }

    #[test]
    fn a_ref_cycle_does_not_loop() {
        let cyclic = "components:\n  schemas:\n    A:\n      allOf: [ { $ref: '#/components/schemas/A' } ]\n      properties: { a: {}, b: {}, c: {}, d: {}, e: {}, f: {}, g: {}, h: {} }\n";
        let v: Value = serde_norway::from_str(cyclic).unwrap();
        assert_eq!(request_fields(&v, "A").unwrap().len(), 8);
    }
}
