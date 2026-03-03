//! Anthropic Messages API — types and translation to/from OpenAI Chat Completions.
//!
//! This module implements:
//! - `MessagesRequest` / `MessagesResponse` structs matching the Anthropic API.
//! - `MessagesRequest::to_openai_chat()` — translate to `ChatCompletionRequest`.
//! - `from_openai_response()` — translate `ChatCompletionResponse` to `MessagesResponse`.
//! - `translate_sse_stream()` — async SSE adapter (OpenAI stream → Anthropic event stream).
//!
//! References:
//!   https://docs.anthropic.com/en/api/messages
//!   https://docs.anthropic.com/en/api/messages-streaming

use crate::protocols::spec::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Tool,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================
// T-20: Request / Response types
// ============================================================

/// A single content block in an Anthropic message.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
    },
}

/// Content of an Anthropic message: either a plain string or an array of content blocks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl AnthropicContent {
    /// Return the concatenated plain text of this content.
    pub fn as_text(&self) -> String {
        match self {
            AnthropicContent::Text(s) => s.clone(),
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// A message in the Anthropic conversation format.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicMessage {
    /// "user" or "assistant"
    pub role: String,
    pub content: AnthropicContent,
}

/// Optional metadata attached to a Messages request.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AnthropicMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Anthropic tool definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input — equivalent to OpenAI's `function.parameters`.
    pub input_schema: Value,
}

/// POST /v1/messages request body (Anthropic Messages API).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,

    /// Optional system prompt (top-level, not in messages array).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<AnthropicContent>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Top-K sampling (Anthropic extension; vLLM also accepts it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    /// Stop sequences (vs OpenAI's `stop`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,

    #[serde(default)]
    pub stream: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
}

/// Usage block in an Anthropic response.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// POST /v1/messages response body (Anthropic Messages API).
#[derive(Debug, Clone, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub msg_type: String, // "message"
    pub role: String,     // "assistant"
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
}

// ============================================================
// T-21: MessagesRequest → ChatCompletionRequest
// ============================================================

impl MessagesRequest {
    /// Translate an Anthropic `MessagesRequest` into an OpenAI `ChatCompletionRequest`
    /// suitable for forwarding to a vLLM or OpenAI-compatible backend.
    pub fn to_openai_chat(&self) -> ChatCompletionRequest {
        let mut messages: Vec<ChatMessage> = Vec::new();

        // System prompt lives top-level in the Anthropic API; OpenAI puts it in messages[0].
        if let Some(system) = &self.system {
            messages.push(ChatMessage::System {
                role: "system".to_string(),
                content: system.as_text(),
                name: None,
            });
        }

        // Translate user/assistant messages.
        for msg in &self.messages {
            let content_str = msg.content.as_text();
            match msg.role.as_str() {
                "user" => messages.push(ChatMessage::User {
                    role: "user".to_string(),
                    content: crate::protocols::spec::UserMessageContent::Text(content_str),
                    name: None,
                }),
                "assistant" => messages.push(ChatMessage::Assistant {
                    role: "assistant".to_string(),
                    content: Some(content_str),
                    name: None,
                    tool_calls: None,
                    function_call: None,
                    reasoning_content: None,
                }),
                other => {
                    // Unknown role — pass through as a generic user message
                    tracing::warn!("Anthropic translation: unknown role '{}', mapping to user", other);
                    messages.push(ChatMessage::User {
                        role: "user".to_string(),
                        content: crate::protocols::spec::UserMessageContent::Text(content_str),
                        name: None,
                    });
                }
            }
        }

        // Translate tools: Anthropic input_schema → OpenAI function.parameters
        let openai_tools = self.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| Tool {
                    tool_type: "function".to_string(),
                    function: crate::protocols::spec::Function {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone(),
                    },
                })
                .collect::<Vec<_>>()
        });

        // Translate stop_sequences → stop
        let stop = self
            .stop_sequences
            .as_ref()
            .map(|seqs| crate::protocols::spec::StringOrArray::Array(seqs.clone()));

        // metadata.user_id → user
        let user = self
            .metadata
            .as_ref()
            .and_then(|m| m.user_id.clone());

        ChatCompletionRequest {
            model: Some(self.model.clone()),
            messages,
            temperature: self.temperature,
            top_p: self.top_p,
            n: None,
            stream: self.stream,
            stream_options: None,
            stop,
            max_tokens: Some(self.max_tokens),
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user,
            seed: None,
            logprobs: false,
            top_logprobs: None,
            response_format: None,
            tools: openai_tools,
            tool_choice: None, // TODO: translate tool_choice if needed
            parallel_tool_calls: None,
            functions: None,
            function_call: None,
            top_k: self.top_k.map(|k| k as i32),
            min_p: None,
            min_tokens: None,
            regex: None,
            ebnf: None,
            stop_token_ids: None,
            no_stop_trim: false,
            ignore_eos: false,
            add_generation_prompt: true,
            continue_final_message: false,
            skip_special_tokens: true,
            lora_path: None,
            session_params: None,
            separate_reasoning: true,
            stream_reasoning: true,
            chat_template_kwargs: None,
            return_hidden_states: false,
            repetition_penalty: None,
            echo: None,
            reasoning_effort: None,
            include_reasoning: true,
            structured_outputs: None,
        }
    }
}

// ============================================================
// T-22: ChatCompletionResponse → MessagesResponse
// ============================================================

/// Map an OpenAI `finish_reason` to an Anthropic `stop_reason`.
fn map_stop_reason(finish_reason: Option<&str>) -> String {
    match finish_reason {
        Some("stop") | None => "end_turn".to_string(),
        Some("length") => "max_tokens".to_string(),
        Some("tool_calls") | Some("function_call") => "tool_use".to_string(),
        Some("content_filter") => "end_turn".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Extract content blocks from an OpenAI `ChatChoice`.
fn choice_to_content_blocks(choice: &ChatChoice) -> Vec<ContentBlock> {
    match &choice.message {
        ChatMessage::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let mut blocks: Vec<ContentBlock> = Vec::new();

            if let Some(text) = content.as_deref().filter(|s| !s.is_empty()) {
                blocks.push(ContentBlock::Text {
                    text: text.to_string(),
                });
            }

            if let Some(calls) = tool_calls {
                for call in calls {
                    let input: Value = call
                        .function
                        .arguments
                        .as_deref()
                        .and_then(|args| serde_json::from_str(args).ok())
                        .unwrap_or(Value::Object(serde_json::Map::new()));
                    let name = call.function.name.clone();
                    blocks.push(ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name,
                        input,
                    });
                }
            }

            if blocks.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: String::new(),
                });
            }
            blocks
        }
        other => {
            // Non-assistant message — extract whatever text we can
            let text = match other {
                ChatMessage::User { content, .. } => match content {
                    crate::protocols::spec::UserMessageContent::Text(s) => s.clone(),
                    crate::protocols::spec::UserMessageContent::Parts(_) => String::new(),
                },
                ChatMessage::System { content, .. } => content.clone(),
                _ => String::new(),
            };
            vec![ContentBlock::Text { text }]
        }
    }
}

/// Translate an OpenAI `ChatCompletionResponse` to an Anthropic `MessagesResponse`.
pub fn from_openai_response(
    openai: &ChatCompletionResponse,
    original_model: &str,
) -> MessagesResponse {
    let choice = openai.choices.first();

    let content = choice
        .map(choice_to_content_blocks)
        .unwrap_or_else(|| vec![ContentBlock::Text { text: String::new() }]);

    let stop_reason = choice
        .and_then(|c| c.finish_reason.as_deref())
        .map(|r| map_stop_reason(Some(r)))
        .unwrap_or_else(|| "end_turn".to_string());

    let usage = openai
        .usage
        .as_ref()
        .map(|u| AnthropicUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        })
        .unwrap_or(AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
        });

    // Prefer the model from the response; fall back to what the client sent.
    let model = if openai.model.is_empty() {
        original_model.to_string()
    } else {
        openai.model.clone()
    };

    MessagesResponse {
        id: format!("msg_{}", &openai.id),
        msg_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model,
        stop_reason,
        stop_sequence: None,
        usage,
    }
}

// ============================================================
// T-23: SSE state machine — OpenAI stream → Anthropic event stream
// ============================================================

/// State machine that converts OpenAI SSE chunks to Anthropic SSE events.
///
/// Anthropic streaming protocol (ordered events per response):
/// 1. `message_start`         — sent once at the start
/// 2. `content_block_start`   — once per content block (index 0 for text)
/// 3. `ping`                  — optional keep-alive
/// 4. `content_block_delta`   — per text chunk
/// 5. `content_block_stop`    — once per content block
/// 6. `message_delta`         — stop_reason + usage
/// 7. `message_stop`          — final event
#[derive(Debug, Default, PartialEq)]
pub enum SseState {
    #[default]
    Initial,
    ContentOpen,
    Done,
}

/// Translate a single OpenAI SSE `data:` line into zero or more Anthropic SSE events.
///
/// Returns a `Vec<String>` where each element is a complete `event:\ndata:\n\n` block.
pub fn translate_sse_chunk(
    data_line: &str,
    model: &str,
    msg_id: &str,
    state: &mut SseState,
) -> Vec<String> {
    let mut out = Vec::new();

    // OpenAI signals end-of-stream with `data: [DONE]`
    if data_line.trim() == "[DONE]" {
        if *state == SseState::ContentOpen {
            out.push(sse_event(
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": 0}),
            ));
        }
        out.push(sse_event(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 0}
            }),
        ));
        out.push(sse_event(
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        ));
        *state = SseState::Done;
        return out;
    }

    // Parse the OpenAI chunk JSON
    let chunk: Value = match serde_json::from_str(data_line) {
        Ok(v) => v,
        Err(_) => return out, // ignore malformed lines
    };

    // Emit `message_start` and `content_block_start` on the first valid chunk
    if *state == SseState::Initial {
        out.push(sse_event(
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{}", msg_id),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ));
        out.push(sse_event(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        *state = SseState::ContentOpen;
    }

    // Extract the delta text from the OpenAI chunk
    let delta_text = chunk
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let finish_reason = chunk
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());

    if !delta_text.is_empty() {
        out.push(sse_event(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": delta_text}
            }),
        ));
    }

    if let Some(reason) = finish_reason {
        let stop_reason = map_stop_reason(Some(reason));
        out.push(sse_event(
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
        out.push(sse_event(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"output_tokens": 0}
            }),
        ));
        out.push(sse_event(
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        ));
        *state = SseState::Done;
    }

    out
}

/// Format an event name + JSON data as an SSE block.
fn sse_event(event: &str, data: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event,
        serde_json::to_string(data).unwrap_or_default()
    )
}

// ============================================================
// T-26: Unit tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::spec::Usage;

    fn make_request(system: Option<&str>, stream: bool) -> MessagesRequest {
        MessagesRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello!".to_string()),
            }],
            max_tokens: 256,
            system: system.map(|s| AnthropicContent::Text(s.to_string())),
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream,
            tools: None,
            tool_choice: None,
            metadata: None,
        }
    }

    #[test]
    fn test_to_openai_chat_no_system() {
        let req = make_request(None, false);
        let oai = req.to_openai_chat();
        assert_eq!(oai.messages.len(), 1);
        assert!(matches!(oai.messages[0], ChatMessage::User { .. }));
        assert_eq!(oai.max_tokens, Some(256));
        assert_eq!(oai.model.as_deref(), Some("claude-3-5-sonnet-20241022"));
        assert!(!oai.stream);
    }

    #[test]
    fn test_to_openai_chat_with_system() {
        let req = make_request(Some("You are helpful."), false);
        let oai = req.to_openai_chat();
        assert_eq!(oai.messages.len(), 2);
        assert!(matches!(oai.messages[0], ChatMessage::System { .. }));
        assert!(matches!(oai.messages[1], ChatMessage::User { .. }));
    }

    #[test]
    fn test_to_openai_chat_stop_sequences() {
        let mut req = make_request(None, false);
        req.stop_sequences = Some(vec!["END".to_string(), "STOP".to_string()]);
        let oai = req.to_openai_chat();
        assert!(matches!(
            oai.stop,
            Some(crate::protocols::spec::StringOrArray::Array(_))
        ));
    }

    #[test]
    fn test_to_openai_chat_metadata_user() {
        let mut req = make_request(None, false);
        req.metadata = Some(AnthropicMetadata {
            user_id: Some("user-42".to_string()),
        });
        let oai = req.to_openai_chat();
        assert_eq!(oai.user.as_deref(), Some("user-42"));
    }

    #[test]
    fn test_from_openai_response_basic() {
        let oai = ChatCompletionResponse {
            id: "chatcmpl-abc".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "llama-3".to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage::Assistant {
                    role: "assistant".to_string(),
                    content: Some("Hi there!".to_string()),
                    name: None,
                    tool_calls: None,
                    function_call: None,
                    reasoning_content: None,
                },
                logprobs: None,
                finish_reason: Some("stop".to_string()),
                matched_stop: None,
                hidden_states: None,
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
        };

        let anthro = from_openai_response(&oai, "claude-3-5-sonnet-20241022");
        assert_eq!(anthro.role, "assistant");
        assert_eq!(anthro.stop_reason, "end_turn");
        assert_eq!(anthro.usage.input_tokens, 10);
        assert_eq!(anthro.usage.output_tokens, 5);
        assert!(anthro.id.starts_with("msg_"));

        // Content should be a single text block
        assert_eq!(anthro.content.len(), 1);
        if let ContentBlock::Text { text } = &anthro.content[0] {
            assert_eq!(text, "Hi there!");
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn test_finish_reason_mapping() {
        assert_eq!(map_stop_reason(Some("stop")), "end_turn");
        assert_eq!(map_stop_reason(Some("length")), "max_tokens");
        assert_eq!(map_stop_reason(Some("tool_calls")), "tool_use");
        assert_eq!(map_stop_reason(None), "end_turn");
    }

    #[test]
    fn test_sse_translate_chunk() {
        let mut state = SseState::default();
        let chunk = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let events = translate_sse_chunk(chunk, "llama-3", "abc123", &mut state);
        // First chunk should emit message_start, content_block_start, content_block_delta
        assert!(events.iter().any(|e| e.contains("message_start")));
        assert!(events.iter().any(|e| e.contains("content_block_start")));
        assert!(events.iter().any(|e| e.contains("Hello")));
        assert_eq!(state, SseState::ContentOpen);
    }

    #[test]
    fn test_sse_done_event() {
        let mut state = SseState::ContentOpen;
        let events = translate_sse_chunk("[DONE]", "llama-3", "abc", &mut state);
        assert!(events.iter().any(|e| e.contains("message_stop")));
        assert_eq!(state, SseState::Done);
    }

    #[test]
    fn test_sse_finish_reason_closes_stream() {
        let mut state = SseState::ContentOpen;
        let chunk = r#"{"choices":[{"delta":{"content":""},"finish_reason":"stop"}]}"#;
        let events = translate_sse_chunk(chunk, "llama-3", "abc", &mut state);
        assert!(events.iter().any(|e| e.contains("end_turn")));
        assert_eq!(state, SseState::Done);
    }

    #[test]
    fn test_tools_translation() {
        let mut req = make_request(None, false);
        req.tools = Some(vec![AnthropicTool {
            name: "get_weather".to_string(),
            description: Some("Get current weather".to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }]);
        let oai = req.to_openai_chat();
        let tools = oai.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "get_weather");
    }
}
