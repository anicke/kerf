//! MCP tool definitions, generated from the JSON Schema of `ops::Request`.
//!
//! Every operation becomes one tool named after its `op`; its description
//! and parameter schema come straight from the request type, so the tools
//! can never drift from what the engine accepts.

use std::sync::Arc;

use kerf_core::ops::Request;
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{json, Map, Value};

/// Descriptions for parameters that many operations share.
const COMMON: &[(&str, &str)] = &[
    (
        "tracks",
        "Track ids to act on (from `info`); all tracks when omitted.",
    ),
    ("start", "Range start (default: the beginning)."),
    ("end", "Range end (default: the end of the project)."),
    ("at", "Timeline position."),
    (
        "path",
        "File or folder path. Relative paths are resolved against the first allowed folder.",
    ),
    ("force", "Replace existing files."),
    (
        "dry_run",
        "Report what would be written without writing anything.",
    ),
    ("discard", "Drop unsaved changes to the current project."),
];

/// Operations that never change the project or any file.
const READ_ONLY: &[&str] = &[
    "info",
    "analyze",
    "history",
    "album_summary",
    "find_silences",
    "waveform",
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

/// One tool per operation.
pub fn tools() -> Vec<Tool> {
    let schema = schemars::schema_for!(Request);
    let root = serde_json::to_value(&schema).expect("schema serializes");
    let defs = root.get("$defs").cloned();
    let variants = root
        .get("oneOf")
        .and_then(Value::as_array)
        .expect("Request is an internally tagged enum");

    variants
        .iter()
        .filter_map(|v| {
            let mut v = v.as_object()?.clone();
            let props = v.get_mut("properties")?.as_object_mut()?;
            let op_schema = props.remove("op")?;
            let name = op_schema
                .get("const")
                .or_else(|| op_schema.get("enum").and_then(|e| e.get(0)))?
                .as_str()?
                .to_string();
            for (key, text) in COMMON {
                if let Some(Value::Object(p)) = props.get_mut(*key) {
                    p.entry("description").or_insert_with(|| json!(text));
                }
            }
            if let Some(Value::Array(req)) = v.get_mut("required") {
                req.retain(|r| r != "op");
                if req.is_empty() {
                    v.remove("required");
                }
            }
            let description = v
                .remove("description")
                .and_then(|d| d.as_str().map(String::from))
                .unwrap_or_else(|| title(&name));
            let mut input: JsonObject = Map::new();
            input.insert("type".into(), json!("object"));
            for (k, val) in v {
                if k != "type" {
                    input.insert(k, val);
                }
            }
            if let Some(d) = &defs {
                input.insert("$defs".into(), d.clone());
            }

            let read_only = READ_ONLY.contains(&name.as_str());
            let annotations = ToolAnnotations::with_title(title(&name))
                .read_only(read_only)
                .destructive(WRITES_FILES.contains(&name.as_str()))
                .open_world(false);
            Some(Tool::new(name, description, Arc::new(input)).annotate(annotations))
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
            "set_album",
            "album_summary",
            "album_loudness",
            "export",
        ] {
            assert!(tools.iter().any(|t| t.name == name), "missing {name}");
        }
        assert_eq!(tools.len(), 34, "a new operation needs a test entry");
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
