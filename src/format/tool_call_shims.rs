use serde_json::{Map, Value};

/// Apply compatibility remaps for known Claude Code tools.
///
/// Normalizes common Gemini/CloudCode argument shape drift so tool calls remain
/// valid for clients expecting Claude's canonical tool schemas.
pub fn remap_tool_call_args(tool_name: &str, args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };

    // Tool-call IDs are tracked separately in the envelope; not a tool arg.
    obj.remove("id");

    match normalized_tool_name(tool_name).as_str() {
        "grep" | "glob" => remap_pattern_and_path(obj),
        "read" => remap_read_path(obj),
        _ => {}
    }
}

fn normalized_tool_name(tool_name: &str) -> String {
    tool_name
        .split(|c| ['/', ':', '.'].contains(&c))
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or(tool_name)
        .to_ascii_lowercase()
}

fn remap_pattern_and_path(obj: &mut Map<String, Value>) {
    if !obj.contains_key("pattern")
        && let Some(desc) = obj.remove("description")
    {
        obj.insert("pattern".to_string(), desc);
    }
    if !obj.contains_key("pattern")
        && let Some(query) = obj.remove("query")
    {
        obj.insert("pattern".to_string(), query);
    }

    if !obj.contains_key("path") {
        if let Some(paths) = obj.remove("paths") {
            obj.insert(
                "path".to_string(),
                Value::String(extract_first_path_or_default(&paths)),
            );
        } else {
            obj.insert("path".to_string(), Value::String(".".to_string()));
        }
    } else {
        obj.remove("paths");
    }
}

fn remap_read_path(obj: &mut Map<String, Value>) {
    if !obj.contains_key("file_path")
        && let Some(path) = obj.remove("path")
    {
        obj.insert("file_path".to_string(), path);
    }
}

fn extract_first_path_or_default(value: &Value) -> String {
    if let Some(path) = value.as_str() {
        return path.to_string();
    }
    if let Some(array) = value.as_array()
        && let Some(path) = array.first().and_then(|v| v.as_str())
    {
        return path.to_string();
    }
    ".".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remap_grep_query_and_paths() {
        let mut args = serde_json::json!({
            "id": "call_1",
            "query": "needle",
            "paths": ["src", "tests"]
        });

        remap_tool_call_args("Grep", &mut args);

        assert_eq!(args["pattern"], serde_json::json!("needle"));
        assert_eq!(args["path"], serde_json::json!("src"));
        assert!(args.get("query").is_none());
        assert!(args.get("paths").is_none());
        assert!(args.get("id").is_none());
    }

    #[test]
    fn test_remap_read_path_to_file_path() {
        let mut args = serde_json::json!({
            "path": "README.md"
        });

        remap_tool_call_args("read", &mut args);

        assert_eq!(args["file_path"], serde_json::json!("README.md"));
        assert!(args.get("path").is_none());
    }
}
