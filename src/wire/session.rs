//! Session shapes for the three adapters.
//!
//! The load-bearing property here is structural: [`FramelessSession`] has no
//! `type` field to serialize, so it cannot acquire one. Adding a top-level
//! `"type": "quicksilver"` to a Frameless body converts it into a V1 body and is
//! the mistake that the 2026-07-24 defect looked like but was not — the real
//! cause was a dropped header, and "fixing" the body would have entrenched it.

use serde::Serialize;
use serde_json::{Map, Value};

/// Bytes per estimated token, matching upstream `approx_token_count`
/// (`codex-utils-string/src/truncate.rs`), which divides the UTF-8 **byte**
/// length — not the character count — by four. Counting characters instead
/// under-counts multi-byte text by up to 4x and would let an over-limit payload
/// through.
pub const APPROX_BYTES_PER_TOKEN: usize = 4;

/// `initial_items` limits, enforced rather than assumed (docs/000 §3.2).
pub const MAX_INITIAL_ITEMS: usize = 128;
pub const MAX_ESTIMATED_TOKENS_PER_ITEM: usize = 8_192;
pub const MAX_ESTIMATED_TOKENS_TOTAL: usize = 8_192;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("initial_items may not exceed {MAX_INITIAL_ITEMS} entries (got {0})")]
    TooManyItems(usize),
    #[error("an initial_items entry exceeds {MAX_ESTIMATED_TOKENS_PER_ITEM} estimated tokens")]
    ItemTooLarge,
    #[error("initial_items exceeds {MAX_ESTIMATED_TOKENS_TOTAL} estimated tokens in total")]
    TotalTooLarge,
}

// ---------------------------------------------------------------------------
// Frameless (v3 / GPT-Live)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Delegation {
    /// Not a session type. This `type` lives *inside* `delegation`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl Default for Delegation {
    fn default() -> Self {
        Self { kind: "client" }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FramelessAudioOutput {
    pub voice: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FramelessAudio {
    pub output: FramelessAudioOutput,
}

/// The Frameless session body.
///
/// Field order matches the upstream builder's serialization so a byte-for-byte
/// comparison against the pinned fixture is meaningful.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FramelessSession {
    // NOTE: there is deliberately no `type` field on this struct, and no
    // `#[serde(flatten)]` field that could smuggle one in.
    pub audio: FramelessAudio,
    pub delegation: Delegation,
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Private: the documented limits are enforced by
    /// [`FramelessSession::with_initial_items`], so a caller cannot assign an
    /// oversized list or an empty `Some(vec![])` that would serialize as `[]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_items: Option<Vec<InitialItem>>,
}

impl FramelessSession {
    pub fn new(instructions: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            audio: FramelessAudio {
                output: FramelessAudioOutput {
                    voice: voice.into(),
                },
            },
            delegation: Delegation::default(),
            instructions: instructions.into(),
            model: None,
            initial_items: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Read-only view of the validated items.
    pub fn initial_items(&self) -> Option<&[InitialItem]> {
        self.initial_items.as_deref()
    }

    /// Attach initial history, enforcing the documented limits. An empty list is
    /// omitted entirely rather than serialized as `[]`.
    pub fn with_initial_items(mut self, items: Vec<InitialItem>) -> Result<Self, SessionError> {
        if items.is_empty() {
            self.initial_items = None;
            return Ok(self);
        }
        if items.len() > MAX_INITIAL_ITEMS {
            return Err(SessionError::TooManyItems(items.len()));
        }
        let mut total = 0usize;
        for item in &items {
            let estimate = item.estimated_tokens();
            if estimate > MAX_ESTIMATED_TOKENS_PER_ITEM {
                return Err(SessionError::ItemTooLarge);
            }
            total = total.saturating_add(estimate);
        }
        if total > MAX_ESTIMATED_TOKENS_TOTAL {
            return Err(SessionError::TotalTooLarge);
        }
        self.initial_items = Some(items);
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Developer,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Developer => "developer",
            Self::Assistant => "assistant",
        }
    }

    /// `user` and `developer` are inputs; `assistant` is output.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::User | Self::Developer => "input_text",
            Self::Assistant => "output_text",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InitialItemContent {
    #[serde(rename = "type")]
    pub content_type: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InitialItem {
    #[serde(rename = "type")]
    pub item_type: &'static str,
    pub role: &'static str,
    pub content: Vec<InitialItemContent>,
}

impl InitialItem {
    /// Named `from_message` to match the contract in docs/040.
    pub fn from_message(role: Role, text: impl Into<String>) -> Self {
        Self {
            item_type: "message",
            role: role.as_str(),
            content: vec![InitialItemContent {
                content_type: role.content_type(),
                text: text.into(),
            }],
        }
    }

    /// Estimated tokens, matching upstream `approx_token_count`: UTF-8 byte
    /// length divided by four, summed across the item's content parts.
    fn estimated_tokens(&self) -> usize {
        self.content
            .iter()
            .map(|c| c.text.len().div_ceil(APPROX_BYTES_PER_TOKEN))
            .sum()
    }
}

// ---------------------------------------------------------------------------
// V1 quicksilver
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum V1SessionType {
    Quicksilver,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct V1AudioFormat {
    #[serde(rename = "type")]
    pub format_type: &'static str,
    pub rate: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct V1AudioInput {
    pub format: V1AudioFormat,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct V1AudioOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<V1AudioFormat>,
    pub voice: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct V1Audio {
    pub input: V1AudioInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<V1AudioOutput>,
}

/// Realtime PCM sample rate used by the V1 session.
pub const REALTIME_AUDIO_SAMPLE_RATE: u32 = 24_000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct V1Session {
    #[serde(rename = "type")]
    pub session_type: V1SessionType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    pub audio: V1Audio,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

impl V1Session {
    pub fn new(instructions: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            session_type: V1SessionType::Quicksilver,
            id: None,
            model: None,
            instructions: Some(instructions.into()),
            output_modalities: None,
            audio: V1Audio {
                input: V1AudioInput {
                    format: V1AudioFormat {
                        format_type: "audio_pcm",
                        rate: REALTIME_AUDIO_SAMPLE_RATE,
                    },
                },
                output: Some(V1AudioOutput {
                    format: None,
                    voice: voice.into(),
                }),
            },
            tools: None,
            tool_choice: None,
        }
    }
}

// ---------------------------------------------------------------------------
// RealtimeV2
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeV2Type {
    Realtime,
    Transcription,
}

/// Modeled only to the depth the relay needs: a `type` discriminant plus
/// passthrough. The full conversational contract is reference material in
/// docs/000 §3.3 and is deliberately not built here.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RealtimeV2Session {
    #[serde(rename = "type")]
    session_type: RealtimeV2Type,
    /// Private: a flattened map containing `type` would emit a duplicate key
    /// with `to_string` and silently override the discriminant with `to_value`,
    /// so the reserved key is stripped at construction instead.
    #[serde(flatten)]
    rest: Map<String, Value>,
}

impl RealtimeV2Session {
    /// Reserved keys the passthrough map may not set.
    pub const RESERVED_KEYS: [&'static str; 1] = ["type"];

    pub fn new(session_type: RealtimeV2Type, mut rest: Map<String, Value>) -> Self {
        for key in Self::RESERVED_KEYS {
            rest.remove(key);
        }
        Self { session_type, rest }
    }

    pub fn session_type(&self) -> RealtimeV2Type {
        self.session_type
    }

    pub fn rest(&self) -> &Map<String, Value> {
        &self.rest
    }
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// Remove the top-level `id` from a session object before call-create.
///
/// Applies to every adapter. The identity *headers* stay on the request; only
/// the session JSON loses its `id` (docs/000 §3.4).
pub fn strip_session_id(session: &mut Value) {
    if let Some(object) = session.as_object_mut() {
        object.remove("id");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact JSON the upstream test pins (docs/000 §3.2).
    #[test]
    fn frameless_serialization_matches_the_pinned_fixture() {
        let session = FramelessSession::new("backend prompt\n\nstartup context", "cove")
            .with_model("gpt-live-1-boulder-alpha");
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(
            json,
            r#"{"audio":{"output":{"voice":"cove"}},"delegation":{"type":"client"},"instructions":"backend prompt\n\nstartup context","model":"gpt-live-1-boulder-alpha"}"#
        );
    }

    /// The structural guarantee: a Frameless body has no top-level `type`, and
    /// the `type` it does contain belongs to `delegation`.
    #[test]
    fn a_frameless_session_has_no_top_level_type() {
        let session = FramelessSession::new("i", "cove");
        let value = serde_json::to_value(&session).unwrap();
        assert!(
            value.get("type").is_none(),
            "a top-level type would convert this into a V1 body"
        );
        assert_eq!(value["delegation"]["type"], "client");
    }

    #[test]
    fn an_absent_model_and_empty_initial_items_are_omitted() {
        let session = FramelessSession::new("i", "cove")
            .with_initial_items(vec![])
            .unwrap();
        let value = serde_json::to_value(&session).unwrap();
        assert!(value.get("model").is_none());
        assert!(value.get("initial_items").is_none());
    }

    #[test]
    fn initial_items_map_roles_to_content_types() {
        let items = vec![
            InitialItem::from_message(Role::User, "hello"),
            InitialItem::from_message(Role::Developer, "context"),
            InitialItem::from_message(Role::Assistant, "reply"),
        ];
        let session = FramelessSession::new("i", "cove")
            .with_initial_items(items)
            .unwrap();
        let value = serde_json::to_value(&session).unwrap();
        let items = value["initial_items"].as_array().unwrap();

        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["role"], "developer");
        assert_eq!(items[1]["content"][0]["type"], "input_text");
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[2]["content"][0]["type"], "output_text");
    }

    #[test]
    fn exactly_the_item_limit_is_accepted() {
        let items: Vec<_> = (0..MAX_INITIAL_ITEMS)
            .map(|_| InitialItem::from_message(Role::User, "x"))
            .collect();
        let session = FramelessSession::new("i", "cove")
            .with_initial_items(items)
            .expect("128 items is exactly the limit");
        assert_eq!(session.initial_items().unwrap().len(), MAX_INITIAL_ITEMS);
    }

    #[test]
    fn too_many_initial_items_is_rejected() {
        let items: Vec<_> = (0..=MAX_INITIAL_ITEMS)
            .map(|_| InitialItem::from_message(Role::User, "x"))
            .collect();
        assert_eq!(
            FramelessSession::new("i", "cove")
                .with_initial_items(items)
                .unwrap_err(),
            SessionError::TooManyItems(MAX_INITIAL_ITEMS + 1)
        );
    }

    /// Exactly at the per-item bound passes; one estimated token more fails.
    #[test]
    fn the_per_item_bound_is_exact() {
        let at_limit = "x".repeat(MAX_ESTIMATED_TOKENS_PER_ITEM * APPROX_BYTES_PER_TOKEN);
        assert!(FramelessSession::new("i", "cove")
            .with_initial_items(vec![InitialItem::from_message(Role::User, at_limit)])
            .is_ok());

        let over = "x".repeat(MAX_ESTIMATED_TOKENS_PER_ITEM * APPROX_BYTES_PER_TOKEN + 1);
        let result = FramelessSession::new("i", "cove")
            .with_initial_items(vec![InitialItem::from_message(Role::User, over)]);
        assert!(matches!(result, Err(SessionError::ItemTooLarge)));
    }

    /// Two items, each legal, whose sum crosses the total bound by one token.
    #[test]
    fn the_total_bound_is_exact() {
        let half = "x".repeat(MAX_ESTIMATED_TOKENS_TOTAL / 2 * APPROX_BYTES_PER_TOKEN);
        let items = vec![
            InitialItem::from_message(Role::User, half.clone()),
            InitialItem::from_message(Role::User, half),
        ];
        assert!(FramelessSession::new("i", "cove")
            .with_initial_items(items)
            .is_ok());

        let half = "x".repeat(MAX_ESTIMATED_TOKENS_TOTAL / 2 * APPROX_BYTES_PER_TOKEN);
        let items = vec![
            InitialItem::from_message(Role::User, half.clone()),
            InitialItem::from_message(Role::User, format!("{half}x")),
        ];
        let result = FramelessSession::new("i", "cove").with_initial_items(items);
        assert!(matches!(result, Err(SessionError::TotalTooLarge)));
    }

    /// Upstream counts UTF-8 BYTES, not characters. Counting characters would
    /// under-count three-byte CJK text threefold and admit an over-limit payload.
    #[test]
    fn the_estimator_counts_bytes_not_characters() {
        // 3 bytes per character; enough characters to exceed the bound by bytes
        // while staying far under it by character count.
        let chars = MAX_ESTIMATED_TOKENS_PER_ITEM * APPROX_BYTES_PER_TOKEN / 3 + 1;
        let text = "가".repeat(chars);
        assert!(
            text.chars().count() < MAX_ESTIMATED_TOKENS_PER_ITEM * APPROX_BYTES_PER_TOKEN,
            "the character count alone must look legal"
        );
        let result = FramelessSession::new("i", "cove")
            .with_initial_items(vec![InitialItem::from_message(Role::User, text)]);
        assert!(matches!(result, Err(SessionError::ItemTooLarge)));
    }

    #[test]
    fn an_oversized_item_is_rejected() {
        let huge = "x".repeat(MAX_ESTIMATED_TOKENS_PER_ITEM * 4 + 8);
        let result = FramelessSession::new("i", "cove")
            .with_initial_items(vec![InitialItem::from_message(Role::User, huge)]);
        // matches! rather than unwrap_err so a failure does not dump the payload.
        assert!(matches!(result, Err(SessionError::ItemTooLarge)));
    }

    #[test]
    fn a_v1_session_always_carries_its_type() {
        let session = V1Session::new("instructions", "cove");
        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["type"], "quicksilver");
        assert_eq!(value["audio"]["input"]["format"]["type"], "audio_pcm");
        assert_eq!(value["audio"]["input"]["format"]["rate"], 24_000);
        assert_eq!(value["audio"]["output"]["voice"], "cove");
        // Absent optionals are omitted, not null.
        assert!(value.get("id").is_none());
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn realtime_v2_types_serialize_as_expected() {
        for (variant, expected) in [
            (RealtimeV2Type::Realtime, "realtime"),
            (RealtimeV2Type::Transcription, "transcription"),
        ] {
            let session = RealtimeV2Session::new(variant, Map::new());
            let value = serde_json::to_value(&session).unwrap();
            assert_eq!(value["type"], expected);
        }
    }

    #[test]
    fn realtime_v2_passthrough_fields_survive() {
        let mut rest = Map::new();
        rest.insert("model".into(), Value::String("gpt-realtime-1.5".into()));
        let session = RealtimeV2Session::new(RealtimeV2Type::Realtime, rest);
        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["model"], "gpt-realtime-1.5");
    }

    /// A flattened `type` would emit a duplicate key with `to_string` and
    /// silently override the discriminant with `to_value`.
    #[test]
    fn realtime_v2_passthrough_cannot_override_its_discriminant() {
        let mut rest = Map::new();
        rest.insert("type".into(), Value::String("bogus".into()));
        rest.insert("model".into(), Value::String("kept".into()));
        let session = RealtimeV2Session::new(RealtimeV2Type::Realtime, rest);

        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["type"], "realtime");
        assert_eq!(value["model"], "kept");

        // And no duplicate key survives into the serialized text.
        let text = serde_json::to_string(&session).unwrap();
        assert_eq!(
            text.matches("\"type\"").count(),
            1,
            "duplicate type key: {text}"
        );
        assert!(!text.contains("bogus"));
    }

    #[test]
    fn strip_session_id_removes_only_the_top_level_id() {
        let mut value = serde_json::json!({
            "id": "sess_123",
            "type": "quicksilver",
            "nested": { "id": "keep-me" }
        });
        strip_session_id(&mut value);
        assert!(value.get("id").is_none());
        assert_eq!(value["type"], "quicksilver");
        assert_eq!(value["nested"]["id"], "keep-me");
    }

    #[test]
    fn strip_session_id_leaves_non_objects_alone() {
        let mut value = serde_json::json!("not an object");
        strip_session_id(&mut value);
        assert_eq!(value, serde_json::json!("not an object"));
    }

    /// Every adapter's call-create body loses its session id, not just V1's.
    #[test]
    fn strip_session_id_applies_to_a_frameless_body_too() {
        let session = FramelessSession::new("i", "cove");
        let mut value = serde_json::to_value(&session).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("id".into(), Value::String("sess_1".into()));
        strip_session_id(&mut value);
        assert!(value.get("id").is_none());
        assert!(value.get("type").is_none(), "still no top-level type");
    }
}
