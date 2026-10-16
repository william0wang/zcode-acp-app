//! ACP wire types: `session/update` payloads and content blocks, ported from
//! `src/lib/types.ts`.
//!
//! Deliberately permissive (same doctrine as the TS app): unknown kinds and
//! fields must be ignored. The SessionUpdate union is discriminated by
//! `sessionUpdate` (per the @agentclientprotocol schema), NOT by `kind`; the
//! rest of the object rides along in [`SessionUpdate::rest`] so unknown
//! fields survive a round trip.
//!
//! Server references: `src/handlers/dispatch.ts` (usage_update,
//! config_option_update), `src/handlers/io.ts` (available_commands_update),
//! `src/config/options.ts` (buildConfigOptions).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A generic `session/update` notification. `sessionUpdate` names the kind;
/// everything else is kind-specific and kept raw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
    pub session_update: String,
    /// All fields besides `sessionUpdate`, verbatim.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub rest: Map<String, Value>,
}

impl SessionUpdate {
    pub fn new(session_update: impl Into<String>) -> Self {
        Self {
            session_update: session_update.into(),
            rest: Map::new(),
        }
    }

    pub fn with_rest(session_update: impl Into<String>, rest: Map<String, Value>) -> Self {
        Self {
            session_update: session_update.into(),
            rest,
        }
    }
}

/// ACP text content block. `type` is the wire discriminator ("text").
///
/// Note: modeled as a plain field rather than serde's internal tagging —
/// `tag` on a struct does not deserialize (serde only supports it on enums),
/// and the app's TS shape is exactly this interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpTextContent {
    pub r#type: String,
    pub text: String,
}

/// ACP structured-diff content block. Edit/Write results ship old/new line
/// sets reconstructed from the backend's patch — NOT a unified-diff string.
/// `type` is the wire discriminator ("diff").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpDiffContent {
    pub r#type: String,
    pub path: String,
    /// JSON null when the file did not exist before the edit.
    pub old_text: Option<String>,
    pub new_text: String,
}

fn as_text_content(v: &Value) -> Option<&str> {
    v.as_object().and_then(|o| {
        if o.get("type").and_then(Value::as_str) == Some("text") {
            o.get("text").and_then(Value::as_str)
        } else {
            None
        }
    })
}

/// The bridge wraps plain results as `{type:"content", content:{type:"text"}}`
/// (Bash output, plan text, question text); unwrap those too.
fn unwrap_text(v: &Value) -> Option<String> {
    if let Some(text) = as_text_content(v) {
        return Some(text.to_owned());
    }
    if v.as_object()
        .map(|o| o.get("type").and_then(Value::as_str) == Some("content"))
        .unwrap_or(false)
    {
        if let Some(inner) = v.get("content") {
            if let Some(text) = as_text_content(inner) {
                return Some(text.to_owned());
            }
        }
    }
    None
}

fn is_diff_content(v: &Value) -> bool {
    v.as_object()
        .map(|o| {
            o.get("type").and_then(Value::as_str) == Some("diff")
                && o.get("newText").map(Value::is_string).unwrap_or(false)
                && o.get("path").map(Value::is_string).unwrap_or(false)
        })
        .unwrap_or(false)
}

fn blocks_of(content: &Value) -> Vec<&Value> {
    match content {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

/// Chunk content is a SINGLE ContentBlock on the wire; tool_call_update
/// content is an ARRAY of blocks. Accept both, concatenating every text
/// fragment (the TS `contentText` helper).
pub fn content_text(content: &Value) -> String {
    let mut out = String::new();
    for block in blocks_of(content) {
        if let Some(text) = unwrap_text(block) {
            out.push_str(&text);
        }
    }
    out
}

/// Extract every structured-diff block (the TS `contentDiffBlocks` helper).
pub fn content_diff_blocks(content: &Value) -> Vec<AcpDiffContent> {
    blocks_of(content)
        .into_iter()
        .filter(|b| is_diff_content(b))
        .filter_map(|b| serde_json::from_value(b.clone()).ok())
        .collect()
}

/// One entry of a `config_option_update` / session-load `configOptions`
/// array. Shape matches the bridge's buildConfigOptions output
/// (id: model | mode | thought).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ConfigOptionChoice>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOptionChoice {
    pub value: String,
    pub name: String,
}

/// usage_update session/update payload — the editor's context bar data.
/// Server: `{used, size}` (handlers/dispatch.ts dispatchUsageDelta).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
}

/// available_commands_update entry (ACP AvailableCommand shape): the bridge's
/// slash commands, advertised per session after load (overwrite semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<SlashCommandInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_text_accepts_single_block_and_array() {
        assert_eq!(content_text(&json!({ "type": "text", "text": "hi" })), "hi");
        assert_eq!(
            content_text(&json!([
                { "type": "text", "text": "a" },
                { "type": "content", "content": { "type": "text", "text": "b" } },
                { "type": "diff", "path": "x", "newText": "n" }
            ])),
            "ab"
        );
        assert_eq!(content_text(&Value::Null), "");
    }

    #[test]
    fn content_diff_blocks_filters_non_diff() {
        let blocks = content_diff_blocks(&json!([
            { "type": "text", "text": "no" },
            { "type": "diff", "path": "/a", "oldText": null, "newText": "n" },
            { "type": "diff", "path": "/b", "oldText": "o", "newText": "n" },
            { "type": "diff", "path": 3, "newText": "bad path" }
        ]));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].path, "/a");
        assert_eq!(blocks[0].old_text, None);
        assert_eq!(blocks[1].old_text.as_deref(), Some("o"));
    }
}
