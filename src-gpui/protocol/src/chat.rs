//! Internal chat model (client-side, persisted in session snapshots), ported
//! from `src/lib/types.ts`. Not a wire contract — but it round-trips through
//! the same JSON storage the TS app uses, so field spellings are frozen.

use serde::{Deserialize, Serialize};

use crate::acp::AcpDiffContent;

/// The tool-call card shape of the chat stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallPart {
    pub tool_call_id: String,
    pub tool_name: String,
    pub detail: String,
    pub status: String,
    /// ACP tool kind (execute/edit/read/search/fetch/switch_mode/other…) for
    /// icon + colour mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Raw backend tool name from `_meta.claudeCode.toolName` (e.g. "Bash"),
    /// distinct from `toolName` which holds the wire title "Bash: npm test".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_name: Option<String>,
    /// Replay-only harness fold (`_meta.zcode.kind` on `histfold_`
    /// tool_calls, server 0.6.0): "context-handoff" | "tool-transcript".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Vec<AcpDiffContent>>,
}

/// One rendered part of a chat message (TS `ChatPart` union, tagged by
/// `type`): plain text, thought text, prompt-attachment image, or a tool
/// call card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ChatPart {
    Text {
        text: String,
    },
    Thought {
        text: String,
    },
    /// Prompt attachment echo: `image` holds a display data URL.
    Image {
        image: String,
    },
    ToolCall(ToolCallPart),
}

/// A compressed prompt attachment ready for the wire (ADR 0007): base64
/// without the data-url prefix — exactly the ACP image block payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDraft {
    pub data: String,
    pub mime_type: String,
}

/// A queued prompt: text plus staged attachments (persisted across restarts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptDraft {
    pub text: String,
    pub images: Vec<AttachmentDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub role: ChatRole,
    pub parts: Vec<ChatPart>,
    pub created_at: u64,
    /// Replay-only: harness-injected context handoff rendered collapsed
    /// instead of a wall of user text (`_meta.zcode.collapsed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapsed: Option<bool>,
}

/// Last completed turn's speed readout (title bar): output tokens, wall
/// seconds, and their ratio. Seeded only by a locally-run turn's prompt
/// response; persisted in the session snapshot so switches/reconnects keep it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStats {
    pub tps: f64,
    pub output_tokens: u64,
    pub seconds: f64,
}
