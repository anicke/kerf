//! MCP tool definitions, generated from the JSON Schema of `ops::Request`.
//!
//! Every operation becomes one tool named after its `op`; its description
//! and parameter schema come straight from the request type, so the tools
//! can never drift from what the engine accepts.

use std::sync::Arc;

use kerf_core::ops::describe;
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{json, Map, Value};

/// Operations that never change the project or any file.
const READ_ONLY: &[&str] = &[
    "info",
    "analyze",
    "history",
    "album_summary",
    "find_silences",
    "waveform",
    "spectrogram",
    "compare",
    "copy",
];

/// Operations that may overwrite files (only when `force` is given).
const WRITES_FILES: &[&str] = &["save", "export"];

fn title(op: &str) -> String {
    let mut t = op.replace('_', " ");
    if let Some(c) = t.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    t
}

/// The definitions a schema refers to (`"$ref": "#/$defs/Name"`), directly
/// or through other definitions. Every tool used to carry all of them, which
/// made the tool list several times larger than it needs to be.
fn used_defs(schema: &Value, defs: &Map<String, Value>) -> Map<String, Value> {
    fn refs<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(o) => {
                if let Some(name) = o
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                {
                    out.push(name);
                }
                o.values().for_each(|x| refs(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }
    let mut used = Map::new();
    let mut todo = Vec::new();
    refs(schema, &mut todo);
    while let Some(name) = todo.pop() {
        if used.contains_key(name) {
            continue;
        }
        if let Some(def) = defs.get(name) {
            used.insert(name.to_string(), def.clone());
            refs(def, &mut todo);
        }
    }
    used
}

/// One tool per operation.
pub fn tools() -> Vec<Tool> {
    let describe::RequestSchemas { ops, defs } = describe::request_schemas();
    ops.into_iter()
        .map(|(name, mut v)| {
            let description = v
                .remove("description")
                .and_then(|d| d.as_str().map(String::from))
                .unwrap_or_else(|| title(&name));
            // The server resolves paths inside its allowed folders.
            if let Some(Value::Object(p)) = v
                .get_mut("properties")
                .and_then(|p| p.get_mut("path"))
            {
                if p.get("description").and_then(Value::as_str) == Some("File or folder path.") {
                    p.insert(
                        "description".into(),
                        json!("File or folder path. Relative paths are resolved against the first allowed folder."),
                    );
                }
            }
            let mut input: JsonObject = Map::new();
            input.insert("type".into(), json!("object"));
            for (k, val) in v {
                if k != "type" {
                    input.insert(k, val);
                }
            }
            let used = used_defs(&Value::Object(input.clone()), &defs);
            if !used.is_empty() {
                input.insert("$defs".into(), Value::Object(used));
            }

            let read_only = READ_ONLY.contains(&name.as_str());
            let annotations = ToolAnnotations::with_title(title(&name))
                .read_only(read_only)
                .destructive(WRITES_FILES.contains(&name.as_str()))
                .open_world(false);
            Tool::new(name, description, Arc::new(input)).annotate(annotations)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool<'a>(tools: &'a [Tool], name: &str) -> &'a Tool {
        tools.iter().find(|t| t.name == name).unwrap()
    }

    #[test]
    fn one_tool_per_operation() {
        let tools = tools();
        for name in [
            "new",
            "open",
            "import",
            "save",
            "undo",
            "redo",
            "history",
            "info",
            "analyze",
            "detect_songs",
            "find_silences",
            "waveform",
            "spectrogram",
            "compare",
            "split",
            "delete",
            "clear",
            "crop",
            "insert_silence",
            "copy",
            "cut",
            "paste",
            "gain",
            "normalize",
            "fade_in",
            "fade_out",
            "remove_fades",
            "set_auto_crossfade",
            "add_marker",
            "update_marker",
            "remove_marker",
            "clear_markers",
            "set_markers",
            "set_album",
            "album_summary",
            "album_loudness",
            "export",
        ] {
            assert!(tools.iter().any(|t| t.name == name), "missing {name}");
        }
        assert_eq!(tools.len(), 37, "a new operation needs a test entry");
    }

    #[test]
    fn schemas_describe_parameters_without_the_tag() {
        let tools = tools();
        let split = tool(&tools, "split");
        let s = Value::Object((*split.input_schema).clone());
        assert_eq!(s["type"], "object");
        assert!(s["properties"].get("op").is_none());
        assert_eq!(s["required"], json!(["at"]));
        assert!(s["properties"]["tracks"]["description"].is_string());
        assert!(split
            .description
            .as_deref()
            .unwrap()
            .starts_with("Split clips"));
        assert!(s["$defs"]["Time"].is_object(), "{s}");

        // Flattened export options appear as ordinary parameters.
        let export = Value::Object((*tool(&tools, "export").input_schema).clone());
        for p in [
            "path", "kind", "preset", "format", "dry_run", "force", "start",
        ] {
            assert!(export["properties"].get(p).is_some(), "export lacks {p}");
        }

        assert_eq!(export["additionalProperties"], false, "typos are rejected");
        assert_eq!(export["required"], json!(["path"]));

        let undo = Value::Object((*tool(&tools, "undo").input_schema).clone());
        assert!(undo.get("required").is_none());
    }

    #[test]
    fn tools_carry_only_the_definitions_they_use() {
        let tools = tools();
        for t in &tools {
            let s = Value::Object((*t.input_schema).clone());
            let text = s.to_string();
            let defs = s.get("$defs").and_then(Value::as_object);
            // Every reference resolves...
            for part in text.split("\"#/$defs/").skip(1) {
                let name = part.split('"').next().unwrap();
                assert!(
                    defs.is_some_and(|d| d.contains_key(name)),
                    "{}: unresolved {name}",
                    t.name
                );
            }
            // ...and nothing unreferenced is carried along.
            for name in defs.into_iter().flat_map(|d| d.keys()) {
                assert!(
                    text.contains(&format!("\"#/$defs/{name}\"")),
                    "{}: unused {name}",
                    t.name
                );
            }
        }
        let undo = Value::Object((*tool(&tools, "undo").input_schema).clone());
        assert!(undo.get("$defs").is_none());

        let total: usize = tools
            .iter()
            .map(|t| serde_json::to_string(t).unwrap().len())
            .sum();
        assert!(total < 40_000, "the tool list grew to {total} bytes");
    }

    #[test]
    fn annotations() {
        let tools = tools();
        let a = |n| tool(&tools, n).annotations.clone().unwrap();
        assert_eq!(a("waveform").read_only_hint, Some(true));
        assert_eq!(a("split").read_only_hint, Some(false));
        assert_eq!(
            a("split").destructive_hint,
            Some(false),
            "edits are undoable"
        );
        assert_eq!(a("export").destructive_hint, Some(true));
    }
}
