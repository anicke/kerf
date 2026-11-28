//! Readable descriptions of every operation and its parameters, generated
//! from the JSON Schema of [`Request`] so they always match what the engine
//! accepts. Used by `kerf-cli ops` and the MCP server's tool list.

use serde::Serialize;
use serde_json::{Map, Value};

use super::Request;

/// Descriptions for parameters that many operations share.
pub const COMMON_PARAMS: &[(&str, &str)] = &[
    (
        "tracks",
        "Track ids to act on (from `info`); all tracks when omitted.",
    ),
    ("start", "Range start (default: the beginning)."),
    ("end", "Range end (default: the end of the project)."),
    ("at", "Timeline position."),
    ("path", "File or folder path."),
    ("force", "Replace existing files."),
    (
        "dry_run",
        "Report what would be written without writing anything.",
    ),
    ("discard", "Drop unsaved changes to the current project."),
];

/// One operation.
#[derive(Clone, Debug, Serialize)]
pub struct OpDoc {
    /// The `op` value, e.g. `split`.
    pub name: String,
    pub description: Option<String>,
    pub params: Vec<ParamDoc>,
}

/// One parameter of an operation.
#[derive(Clone, Debug, Serialize)]
pub struct ParamDoc {
    pub name: String,
    /// A short type, e.g. `time`, `number`, `peak|rms|loudness`, `[time]`.
    pub kind: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    pub description: Option<String>,
}

/// The JSON Schema of every request, as `(op name, schema)` with the `op`
/// property removed, plus the shared definitions they refer to.
pub struct RequestSchemas {
    pub ops: Vec<(String, Map<String, Value>)>,
    pub defs: Map<String, Value>,
}

pub fn request_schemas() -> RequestSchemas {
    let root = serde_json::to_value(schemars::schema_for!(Request)).expect("schema serializes");
    let defs = root
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let ops = root
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let mut v = v.as_object()?.clone();
            let props = v.get_mut("properties")?.as_object_mut()?;
            let op = props.remove("op")?;
            let name = op
                .get("const")
                .or_else(|| op.get("enum").and_then(|e| e.get(0)))?
                .as_str()?
                .to_string();
            for (key, text) in COMMON_PARAMS {
                if let Some(Value::Object(p)) = props.get_mut(*key) {
                    p.entry("description")
                        .or_insert_with(|| Value::String(text.to_string()));
                }
            }
            if let Some(Value::Array(req)) = v.get_mut("required") {
                req.retain(|r| r != "op");
                if req.is_empty() {
                    v.remove("required");
                }
            }
            Some((name, v))
        })
        .collect();
    RequestSchemas { ops, defs }
}

/// Every operation, in the order of [`Request`].
pub fn operations() -> Vec<OpDoc> {
    let RequestSchemas { ops, defs } = request_schemas();
    ops.into_iter()
        .map(|(name, s)| {
            let required: Vec<&str> = s
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let params = s
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(k, v)| ParamDoc {
                    name: k.clone(),
                    kind: kind(v, &defs, 0),
                    required: required.contains(&k.as_str()),
                    default: v.get("default").filter(|d| !d.is_null()).cloned(),
                    description: description(v, &defs),
                })
                .collect();
            OpDoc {
                name,
                description: s.get("description").and_then(Value::as_str).map(clean),
                params,
            }
        })
        .collect()
}

/// Join wrapped doc-comment lines.
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn def<'a>(v: &Value, defs: &'a Map<String, Value>) -> Option<(&'a str, &'a Value)> {
    let name = v.get("$ref")?.as_str()?.strip_prefix("#/$defs/")?;
    defs.get_key_value(name).map(|(k, v)| (k.as_str(), v))
}

/// A parameter's own description, or else its type's.
fn description(v: &Value, defs: &Map<String, Value>) -> Option<String> {
    if let Some(d) = v.get("description").and_then(Value::as_str) {
        return Some(clean(d));
    }
    let inner = v
        .get("anyOf")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| x.get("$ref").is_some()))
        .unwrap_or(v);
    def(inner, defs)
        .and_then(|(_, d)| d.get("description"))
        .and_then(Value::as_str)
        .map(clean)
}

fn kind(v: &Value, defs: &Map<String, Value>, depth: usize) -> String {
    if let Some((name, d)) = def(v, defs) {
        if name == "Time" {
            return "time".into();
        }
        return if depth < 2 {
            kind(d, defs, depth + 1)
        } else {
            name.to_lowercase()
        };
    }
    if let Some(c) = v.get("const") {
        return c.as_str().map_or_else(|| c.to_string(), String::from);
    }
    if let Some(e) = v.get("enum").and_then(Value::as_array) {
        return e
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), String::from))
            .collect::<Vec<_>>()
            .join("|");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(alts) = v.get(key).and_then(Value::as_array) {
            let kinds: Vec<String> = alts
                .iter()
                .filter(|a| a.get("type").and_then(Value::as_str) != Some("null"))
                .map(|a| kind(a, defs, depth))
                .collect();
            return kinds.join("|");
        }
    }
    let ty: Vec<&str> = match v.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let ty: Vec<&str> = ty.into_iter().filter(|t| *t != "null").collect();
    match ty.as_slice() {
        ["array"] => format!(
            "[{}]",
            v.get("items")
                .map_or("any".into(), |i| kind(i, defs, depth))
        ),
        ["object"] => match v.get("properties").and_then(Value::as_object) {
            Some(props) => {
                let req: Vec<&str> = v
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let fields: Vec<String> = props
                    .iter()
                    .map(|(k, p)| {
                        let opt = if req.contains(&k.as_str()) { "" } else { "?" };
                        format!("{k}{opt}: {}", kind(p, defs, depth + 1))
                    })
                    .collect();
                format!("{{{}}}", fields.join(", "))
            }
            None => "object".into(),
        },
        ["integer"] => "integer".into(),
        [] => "any".into(),
        t => t.join("|"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(name: &str) -> OpDoc {
        operations().into_iter().find(|o| o.name == name).unwrap()
    }

    fn param<'a>(o: &'a OpDoc, name: &str) -> &'a ParamDoc {
        o.params.iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn operations_and_parameters_read_well() {
        let split = op("split");
        assert!(split
            .description
            .as_deref()
            .unwrap()
            .starts_with("Split clips"));
        let at = param(&split, "at");
        assert_eq!((at.kind.as_str(), at.required), ("time", true));
        assert_eq!(at.description.as_deref(), Some("Timeline position."));
        assert!(!param(&split, "tracks").required);

        let m = op("set_markers");
        assert_eq!(
            param(&m, "markers").kind,
            "[{artist?: string, at: time, isrc?: string, name: string}]"
        );
        assert_eq!(param(&m, "replace").default, Some(Value::Bool(true)));

        let n = op("normalize");
        assert_eq!(param(&n, "mode").kind, "peak|rms|loudness");
        assert!(op("undo").params.is_empty());
        assert!(operations()
            .iter()
            .all(|o| !o.params.iter().any(|p| p.kind == "any")));
    }
}
