//! Fork patch (Automonique): treat `"key": null` in tool-call arguments as
//! "key absent".
//!
//! Some models fill every optional parameter of a tool schema with an explicit
//! JSON `null`. Typed tool inputs then fail to deserialize (`invalid type:
//! null, expected a string`) and the call is wasted, although the intent was
//! plainly "not provided". [`Registry::execute`](super::Registry::execute) is
//! the one place every call goes through, including `batch` sub-calls, so the
//! arguments are normalized there.
//!
//! The rule is deliberately shallow so opaque payloads keep their meaning:
//!
//! * null-valued keys of the top-level argument object are removed;
//! * null-valued keys of objects that are direct elements of a top-level array
//!   are removed (`edits[]`, `todos[]`, `tool_calls[]`, ...);
//! * nothing deeper is touched, and `null` array elements are kept.
//!
//! Custom SDK tools and MCP tools are passed through untouched: their schemas
//! are not ours and `null` can be a deliberate value there.

use serde_json::Value;

fn strip_object_nulls(value: &mut Value) {
    if let Value::Object(map) = value {
        map.retain(|_, member| !member.is_null());
    }
}

/// Remove null-valued argument keys according to the module rule.
pub(crate) fn strip_null_arguments(mut input: Value) -> Value {
    strip_object_nulls(&mut input);
    if let Value::Object(map) = &mut input {
        for member in map.values_mut() {
            if let Value::Array(items) = member {
                items.iter_mut().for_each(strip_object_nulls);
            }
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::strip_null_arguments;
    use serde_json::json;

    #[test]
    fn top_level_null_keys_are_removed() {
        let stripped = strip_null_arguments(json!({
            "command": "ls",
            "timeout": null,
            "run_in_background": null,
            "intent": "List files"
        }));
        assert_eq!(stripped, json!({"command": "ls", "intent": "List files"}));
    }

    #[test]
    fn objects_inside_top_level_arrays_are_normalized() {
        let stripped = strip_null_arguments(json!({
            "tool_calls": [
                {"tool": "read", "intent": null, "parameters": {"file_path": "a", "limit": null}},
                null,
                "text"
            ]
        }));
        assert_eq!(
            stripped,
            json!({
                "tool_calls": [
                    {"tool": "read", "parameters": {"file_path": "a", "limit": null}},
                    null,
                    "text"
                ]
            }),
            "sub-call parameters are normalized when the sub-call is dispatched, not here"
        );
    }

    #[test]
    fn deeper_payloads_and_non_objects_are_preserved() {
        let nested = json!({"state": {"selected": null}, "items": [[{"a": null}]]});
        assert_eq!(strip_null_arguments(nested.clone()), nested);
        assert_eq!(strip_null_arguments(json!(null)), json!(null));
        assert_eq!(strip_null_arguments(json!("raw")), json!("raw"));
        assert_eq!(strip_null_arguments(json!([{"a": null}])), json!([{"a": null}]));
    }
}
