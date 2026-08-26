use std::sync::Arc;

use schemars::JsonSchema;
use serde_json::{Value, json};

fn schema_object(value: Value) -> Arc<serde_json::Map<String, Value>> {
    Arc::new(
        value
            .as_object()
            .cloned()
            .expect("static tool output schema must be an object"),
    )
}

fn derived_schema<T: JsonSchema>() -> Arc<serde_json::Map<String, Value>> {
    schema_object(
        serde_json::to_value(schemars::schema_for!(T))
            .expect("derived tool output schema must serialize"),
    )
}

pub(super) fn typed_output_schema(tool: &str) -> Option<Arc<serde_json::Map<String, Value>>> {
    let derived = match tool {
        "read_file" => Some(derived_schema::<super::filesystem::ReadFileOutput>()),
        "list_directory" => Some(derived_schema::<super::filesystem::ListDirectoryOutput>()),
        "glob" => Some(derived_schema::<super::search::GlobOutput>()),
        "grep" => Some(derived_schema::<super::search::SearchOutput>()),
        "tree" => Some(derived_schema::<super::misc::TreeOutput>()),
        _ => None,
    };
    if derived.is_some() {
        return derived;
    }
    let schema = match tool {
        "chatgpt_turn_init" => json!({
            "type":"object",
            "properties":{
                "status":{"type":"string","enum":["synchronized","soft_error"]},
                "agent_action":{"type":"string","enum":["continue","stop_current_turn"]},
                "soft_error":{
                    "type":["object","null"],
                    "properties":{
                        "code":{"type":"string"},
                        "message":{"type":"string"},
                        "retry_on_next_user_turn":{"type":"boolean"}
                    },
                    "required":["code","message","retry_on_next_user_turn"],
                    "additionalProperties":true
                },
                "identity_mode":{"type":"string"},
                "transport_mode":{"type":"string"},
                "project_key":{"type":["string","null"]},
                "native_project_key":{"type":["string","null"]},
                "effective_project_key":{"type":["string","null"]},
                "alias":{"type":["string","null"]},
                "initialized":{"type":"boolean"},
                "turn_ref":{"type":["string","null"]},
                "previous_turn_ref":{"type":["string","null"]},
                "instruction_hash":{"type":["string","null"]},
                "state_hash":{"type":["string","null"]},
                "instructions_changed":{"type":"boolean"},
                "state_changed":{"type":"boolean"},
                "turn_reused":{"type":"boolean"},
                "workspace_state":{"type":["string","null"],"enum":["new","existing","joined",null]},
                "reused_existing_binding":{"type":"boolean"},
                "joined_existing_alias":{"type":"boolean"},
                "brief":{"type":["string","null"]},
                "state_update":{"type":["string","null"]}
            },
            "required":["status","agent_action","soft_error","identity_mode","transport_mode","project_key","native_project_key","effective_project_key","initialized","turn_ref","previous_turn_ref","instruction_hash","state_hash","instructions_changed","state_changed","turn_reused","workspace_state","reused_existing_binding","joined_existing_alias","brief","state_update"],
            "additionalProperties":true
        }),
        "exec_command" | "write_stdin" => json!({
            "type":"object",
            "properties":{
                "chunk_id":{"type":"string"},
                "session_id":{"type":["string","null"]},
                "exit_code":{"type":["integer","null"]},
                "completion_reason":{"type":"string","enum":["running","exited","signaled","timed_out","cancelled","failed"]},
                "signal":{"type":["integer","null"]},
                "requested_signal":{"type":["string","null"]},
                "error":{"type":["string","null"]},
                "output":{"type":"string"},
                "output_offset":{"type":"integer"},
                "output_next_offset":{"type":"integer"},
                "output_bytes":{"type":"integer"},
                "truncated":{"type":"boolean"},
                "timed_out":{"type":"boolean"},
                "deadline_exceeded":{"type":"boolean"},
                "tty":{"type":"boolean"},
                "continuation":{"type":["string","null"]}
            },
            "required":["chunk_id","exit_code","completion_reason","signal","requested_signal","error","output","output_bytes","output_offset","output_next_offset","truncated","timed_out","deadline_exceeded","tty"],
            "additionalProperties":true
        }),
        "skills_list" => json!({
            "type":"object",
            "properties":{
                "skills":{"type":"array"},
                "warnings":{"type":"array"},
                "progressive_disclosure":{"type":"boolean"},
                "precedence":{"type":"array","items":{"type":"string"}}
            },
            "required":["skills","warnings","progressive_disclosure","precedence"],
            "additionalProperties":true
        }),
        "skills_read" => json!({
            "type":"object",
            "properties":{
                "name":{"type":"string"},
                "resource":{"type":"string"},
                "content":{"type":"string"},
                "offset":{"type":"integer"},
                "shown_bytes":{"type":"integer"},
                "total_bytes":{"type":"integer"},
                "truncated":{"type":"boolean"},
                "next_offset":{"type":["integer","null"]},
                "continuation":{"type":["string","null"]},
                "package_files":{"type":"array","items":{"type":"string"}},
                "package_files_truncated":{"type":"boolean"}
            },
            "required":["name","resource","content","offset","shown_bytes","total_bytes","truncated"],
            "additionalProperties":true
        }),
        "apply_patch" => json!({
            "type":"object",
            "properties":{
                "files":{"type":"array"},
                "count":{"type":"integer"},
                "applied":{"type":"boolean"},
                "transaction":{"type":"string"}
            },
            "required":["files","count","applied"],
            "additionalProperties":true
        }),
        "view_image" => json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"bytes":{"type":"integer"},"mime_type":{"type":"string"}},
            "required":["path","bytes","mime_type"],
            "additionalProperties":true
        }),
        "remember" => json!({
            "type":"object",
            "properties":{"key":{"type":"string"},"saved":{"type":"boolean"},"deleted":{"type":"boolean"}},
            "required":["key","saved","deleted"],
            "additionalProperties":true
        }),
        "recall" => json!({
            "type":"object",
            "additionalProperties":true
        }),
        "update_plan" => {
            json!({"type":"object","properties":{"plan":{},"updated":{"type":"boolean"}},"required":["plan","updated"],"additionalProperties":true})
        }
        _ => return None,
    };
    Some(schema_object(schema))
}
