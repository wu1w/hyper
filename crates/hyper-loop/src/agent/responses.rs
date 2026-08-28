//! xAI / cli-chat-proxy Responses API completer (`POST /v1/responses`).
//!
//! Chat Completions stays in [`super::http::HttpCompleter`]. Grok 4.6 never
//! gets `chat_template_kwargs`, `enable_thinking`, or llama.cpp `id_slot`.

use reqwest::Client;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Mutex;

use super::delta::StreamPaint;
use super::{Completer, HttpCompleter, ModelTurn, TokenSink};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::family::{EndpointCaps, EngineProfile, Family};
use crate::policy::{grok_forwarding_effort, Effort, ThinkPolicy};
use crate::session::{messages_to_responses_input, OfficialCompaction};
use crate::template::ChatMessage;
use crate::tool_calls::ToolCall;
use crate::transport::{GrokTransport, ResolvedTransport, WireFormat};

pub struct ResponsesCompleter {
    client: Client,
    url: String,
    origin: String,
    api_key: String,
    model: String,
    mode: GrokTransport,
    caps: EndpointCaps,
    policy: Mutex<ThinkPolicy>,
    token_sink: Mutex<Option<TokenSink>>,
    cache_key: Mutex<Option<String>>,
    compaction: Mutex<Option<OfficialCompaction>>,
    compaction_skip: Mutex<usize>,
}

impl ResponsesCompleter {
    pub async fn connect(
        cfg: &Config,
        resolved: &ResolvedTransport,
        policy: ThinkPolicy,
    ) -> Result<Self> {
        let client = crate::llm_http::stream_client(cfg)?;
        let model = {
            let m = cfg.server.model.trim();
            if m.is_empty() {
                "grok-4.6".into()
            } else {
                m.to_string()
            }
        };
        let origin = resolved.base_url.trim_end_matches('/').to_string();
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai);
        Ok(Self {
            client,
            url: responses_url(&origin),
            origin,
            api_key: resolved.token().to_string(),
            model,
            mode: resolved.mode,
            caps,
            policy: Mutex::new(policy),
            token_sink: Mutex::new(None),
            cache_key: Mutex::new(None),
            compaction: Mutex::new(None),
            compaction_skip: Mutex::new(0),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn policy(&self) -> ThinkPolicy {
        lock_policy(&self.policy).clone()
    }

    fn token_sink(&self) -> Option<TokenSink> {
        lock_sink(&self.token_sink).clone()
    }
}

impl Completer for ResponsesCompleter {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        let policy = self.policy();
        let sink = self.token_sink();
        let cache_key = lock_str(&self.cache_key).clone();
        let compaction = lock_comp(&self.compaction).clone();
        let skip = *lock_skip(&self.compaction_skip);
        let body = build_responses_body(&ResponsesSpec {
            model: &self.model,
            messages,
            tools,
            stream: sink.is_some(),
            policy: &policy,
            cache_key: cache_key.as_deref(),
            compaction: compaction.as_ref(),
            skip,
        });
        let mut req = self.client.post(&self.url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        req = crate::transport::apply_grok_headers(req, self.mode);
        if let Some(id) = cache_key.as_deref() {
            req = req.header("x-grok-conv-id", id);
        }
        let mut resp = req.send().await.map_err(|e| sanitize_http(e))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(auth_or_status(status, &text));
        }
        if sink.is_some() {
            let turn = read_responses_sse(&mut resp, sink).await?;
            return Ok(turn);
        }
        let v: Value = resp.json().await.map_err(|e| Error::Http(e.to_string()))?;
        if let Some(err) = v.get("error") {
            return Err(Error::Http(format_api_error(err)));
        }
        let turn = turn_from_responses(&v)?;
        paint_server_output(sink.as_ref(), &v);
        paint_clean(sink, &turn);
        Ok(turn)
    }

    fn prefix_meter(&self) -> Option<(Family, crate::policy::TemplateKwargs)> {
        let policy = self.policy();
        Some((self.caps.family, policy.template_kwargs(&self.caps)))
    }

    fn set_policy(&self, p: ThinkPolicy) {
        *lock_policy(&self.policy) = p;
    }

    fn policy(&self) -> Option<ThinkPolicy> {
        Some(self.policy())
    }

    fn set_token_sink(&self, sink: Option<TokenSink>) {
        *lock_sink(&self.token_sink) = sink;
    }

    fn pin_session(&self, session_id: &str) {
        let key = session_id.trim();
        *lock_str(&self.cache_key) = if key.is_empty() {
            None
        } else {
            Some(key.to_string())
        };
    }

    fn set_official_compaction(&self, item: Option<OfficialCompaction>) {
        *lock_comp(&self.compaction) = item;
    }

    fn set_compaction_skip(&self, n: usize) {
        *lock_skip(&self.compaction_skip) = n;
    }

    fn recasts_xai_product(&self) -> bool {
        true
    }

    fn media_caps(&self) -> crate::media::MediaCaps {
        self.caps
            .media_caps(Some((self.origin.clone(), String::new())))
    }
}

/// Chat Completions or Responses. Auth rail (OAuth / API key / forwarding)
/// is independent: Grok-like endpoints use Cursor Responses; Qwen stays Chat.
pub enum TransportCompleter {
    Responses(ResponsesCompleter),
    Chat(HttpCompleter),
}

impl TransportCompleter {
    pub async fn connect(cfg: &Config, policy: ThinkPolicy) -> Result<Self> {
        let resolved = crate::transport::resolve_live(cfg).await?;
        match crate::transport::detect_wire(cfg, &resolved).await {
            WireFormat::Responses => {
                let c = ResponsesCompleter::connect(cfg, &resolved, policy).await?;
                Ok(Self::Responses(c))
            }
            WireFormat::ChatCompletions => {
                Ok(Self::Chat(HttpCompleter::connect(cfg, policy).await?))
            }
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Responses(c) => c.model(),
            Self::Chat(c) => c.model(),
        }
    }

    pub fn wire(&self) -> &'static str {
        match self {
            Self::Responses(_) => WireFormat::Responses.as_str(),
            Self::Chat(_) => WireFormat::ChatCompletions.as_str(),
        }
    }

    pub fn effort_label(&self) -> Option<&'static str> {
        Completer::policy(self)
            .and_then(|p| p.effort)
            .map(|e| e.as_str())
    }
}

impl Completer for TransportCompleter {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        match self {
            Self::Responses(c) => c.complete(messages, tools).await,
            Self::Chat(c) => c.complete(messages, tools).await,
        }
    }

    fn prefix_meter(&self) -> Option<(Family, crate::policy::TemplateKwargs)> {
        match self {
            Self::Responses(c) => c.prefix_meter(),
            Self::Chat(c) => c.prefix_meter(),
        }
    }

    fn set_policy(&self, p: ThinkPolicy) {
        match self {
            Self::Responses(c) => Completer::set_policy(c, p),
            Self::Chat(c) => Completer::set_policy(c, p),
        }
    }

    fn policy(&self) -> Option<ThinkPolicy> {
        match self {
            Self::Responses(c) => Completer::policy(c),
            Self::Chat(c) => Completer::policy(c),
        }
    }

    fn set_token_sink(&self, sink: Option<TokenSink>) {
        match self {
            Self::Responses(c) => c.set_token_sink(sink),
            Self::Chat(c) => c.set_token_sink(sink),
        }
    }

    fn pin_session(&self, session_id: &str) {
        match self {
            Self::Responses(c) => c.pin_session(session_id),
            Self::Chat(c) => c.pin_session(session_id),
        }
    }

    fn set_official_compaction(&self, item: Option<OfficialCompaction>) {
        match self {
            Self::Responses(c) => c.set_official_compaction(item),
            Self::Chat(_) => {}
        }
    }

    fn set_compaction_skip(&self, n: usize) {
        match self {
            Self::Responses(c) => c.set_compaction_skip(n),
            Self::Chat(_) => {}
        }
    }

    fn recasts_xai_product(&self) -> bool {
        match self {
            Self::Responses(_) => true,
            Self::Chat(c) => Completer::recasts_xai_product(c),
        }
    }

    fn set_low_precision(&self, on: bool) {
        match self {
            Self::Responses(_) => {}
            Self::Chat(c) => c.set_low_precision(on),
        }
    }

    fn media_caps(&self) -> crate::media::MediaCaps {
        match self {
            Self::Responses(c) => Completer::media_caps(c),
            Self::Chat(c) => Completer::media_caps(c),
        }
    }
}

pub(crate) struct ResponsesSpec<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [Value]>,
    pub stream: bool,
    pub policy: &'a ThinkPolicy,
    pub cache_key: Option<&'a str>,
    pub compaction: Option<&'a OfficialCompaction>,
    pub skip: usize,
}

pub(crate) fn build_responses_body(spec: &ResponsesSpec<'_>) -> Value {
    let mut root = Map::new();
    root.insert("model".into(), json!(Family::wire_model_id(spec.model)));
    root.insert("stream".into(), json!(spec.stream));
    root.insert("store".into(), json!(false));
    if spec.policy.max_tokens > 0 {
        root.insert(
            "max_output_tokens".into(),
            json!(spec.policy.max_tokens.max(1)),
        );
    }

    let effort = grok_forwarding_effort(spec.policy, spec.model);
    root.insert("reasoning".into(), json!({ "effort": effort }));

    let (instructions, input) = split_instructions(spec.messages, spec.compaction, spec.skip);
    if let Some(ins) = instructions {
        root.insert("instructions".into(), json!(ins));
    }
    root.insert("input".into(), Value::Array(input));

    if let Some(tools) = spec.tools {
        if !tools.is_empty() {
            let mapped = map_responses_tools(tools);
            root.insert("tools".into(), Value::Array(mapped));
            root.insert("parallel_tool_calls".into(), json!(true));
        }
    }
    if let Some(key) = spec.cache_key.map(str::trim).filter(|s| !s.is_empty()) {
        root.insert("prompt_cache_key".into(), json!(key));
    }
    Value::Object(root)
}

fn split_instructions(
    messages: &[ChatMessage],
    compaction: Option<&OfficialCompaction>,
    skip: usize,
) -> (Option<String>, Vec<Value>) {
    let mut systems = Vec::new();
    let rest: Vec<ChatMessage> = messages
        .iter()
        .filter(|m| {
            if m.role == "system" {
                let t = crate::platform_prefix::wash_platform_injection(
                    m.content.as_deref().unwrap_or(""),
                );
                let t = t.trim();
                if !t.is_empty() {
                    systems.push(t.to_string());
                }
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    let rest: Vec<ChatMessage> = if compaction.is_some() {
        rest.into_iter().skip(skip).collect()
    } else {
        rest
    };
    let instructions = if systems.is_empty() {
        None
    } else {
        Some(systems.join("\n\n"))
    };
    let mut input = Vec::new();
    if let Some(c) = compaction {
        input.push(c.as_input_item());
    }
    input.extend(messages_to_responses_input(&rest));
    (instructions, input)
}

/// Host `web_search` / `x_search` / `image_generation` plus client functions.
/// Drop client `WebSearch` — mixing it with the host tool makes grok-4.6 call both.
fn map_responses_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = crate::tools_schema::xai_server_search_tools();
    let mut seen_host: HashSet<String> = ["web_search", "x_search", "image_generation"]
        .into_iter()
        .map(str::to_string)
        .collect();
    for t in tools {
        let flat = flatten_tool(t);
        let ty = flat
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        if ty != "function" {
            if seen_host.insert(ty.to_string()) {
                out.push(flat);
            }
            continue;
        }
        let name = flat.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() || crate::tools_schema::is_client_web_search_name(name) {
            continue;
        }
        out.push(flat);
    }
    out
}

fn flatten_tool(t: &Value) -> Value {
    if t.get("function").is_none() {
        if t.get("name").is_some() {
            return t.clone();
        }
        let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !ty.is_empty() && ty != "function" {
            return t.clone();
        }
    }
    let f = &t["function"];
    let mut m = Map::new();
    m.insert("type".into(), json!("function"));
    m.insert("name".into(), f.get("name").cloned().unwrap_or(json!("")));
    if let Some(d) = f.get("description") {
        m.insert("description".into(), d.clone());
    } else if let Some(d) = t.get("description") {
        m.insert("description".into(), d.clone());
    }
    if let Some(p) = f.get("parameters") {
        m.insert("parameters".into(), p.clone());
    } else if let Some(p) = t.get("parameters") {
        m.insert("parameters".into(), p.clone());
    }
    Value::Object(m)
}

pub(crate) fn responses_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1") || b.contains("/v1/") {
        format!("{b}/responses")
    } else {
        format!("{b}/v1/responses")
    }
}

fn turn_from_responses(v: &Value) -> Result<ModelTurn> {
    if let Some(err) = v.get("error") {
        return Err(Error::Http(format_api_error(err)));
    }
    let output = v
        .get("output")
        .or_else(|| v.pointer("/response/output"))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let mut acc = ResponsesAcc::default();
    acc.apply_output(&output);
    acc.apply_usage(v);
    acc.into_turn()
}

#[derive(Default)]
struct ResponsesAcc {
    content: String,
    reasoning: String,
    calls: Vec<PendingCall>,
    images: Vec<crate::media::MediaPart>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: Option<u64>,
}

#[derive(Default, Clone)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

impl ResponsesAcc {
    fn apply_event(&mut self, ev: &Value) {
        let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "response.output_text.delta" | "response.text.delta" => {
                if let Some(d) = delta_str(ev) {
                    self.content.push_str(d);
                }
            }
            "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning.delta" => {
                if let Some(d) = delta_str(ev) {
                    self.reasoning.push_str(d);
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = ev.get("item") {
                    self.apply_item(item);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(d) = delta_str(ev) {
                    let id = ev.get("item_id").and_then(|x| x.as_str()).unwrap_or("");
                    self.append_args(id, d);
                }
            }
            ty if ty.starts_with("response.image_generation_call") => {
                self.take_image_result(ev.get("result"));
                if let Some(item) = ev.get("item") {
                    self.apply_item(item);
                }
            }
            "response.completed" => {
                if let Some(resp) = ev.get("response") {
                    if let Some(output) = resp.get("output") {
                        let mut fresh = ResponsesAcc::default();
                        fresh.apply_output(output);
                        fresh.apply_usage(resp);
                        if !fresh.content.is_empty() {
                            self.content = fresh.content;
                        }
                        if !fresh.reasoning.is_empty() {
                            self.reasoning = fresh.reasoning;
                        }
                        if !fresh.calls.is_empty() {
                            self.calls = fresh.calls;
                        }
                        if !fresh.images.is_empty() {
                            self.images = fresh.images;
                        }
                        self.prompt_tokens = fresh.prompt_tokens;
                        self.completion_tokens = fresh.completion_tokens;
                        self.cached_tokens = fresh.cached_tokens;
                    } else {
                        self.apply_usage(resp);
                    }
                }
            }
            "error" => {}
            _ => {
                if let Some(item) = ev.get("item") {
                    self.apply_item(item);
                }
            }
        }
        if let Some(err) = ev.get("error") {
            if !err.is_null() {
                // surfaced by caller
            }
        }
    }

    fn apply_output(&mut self, output: &Value) {
        let Some(arr) = output.as_array() else {
            return;
        };
        for item in arr {
            self.apply_item(item);
        }
    }

    fn apply_item(&mut self, item: &Value) {
        let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "message" | "output_text" => {
                merge_snapshot(&mut self.content, &item_text(item));
            }
            "reasoning" => {
                merge_snapshot(&mut self.reasoning, &item_text(item));
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = match item.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                if !name.is_empty() {
                    self.upsert_call(id, name, arguments);
                }
            }
            // Host-executed. Must not become client ToolCalls (x_keyword_search
            // is not in our dispatcher).
            "web_search_call"
            | "x_search_call"
            | "custom_tool_call"
            | "code_interpreter_call"
            | "file_search_call"
            | "mcp_call" => {}
            "image_generation_call" | "output_image" => {
                self.take_image_result(item.get("result"));
                if let Some(b64) = item.get("b64_json").or_else(|| item.get("image_url")) {
                    self.take_image_result(Some(b64));
                }
            }
            _ => {
                if item.get("content").is_some() {
                    self.content.push_str(&item_text(item));
                }
            }
        }
    }

    fn upsert_call(&mut self, id: String, name: String, arguments: String) {
        if let Some(c) = self
            .calls
            .iter_mut()
            .find(|c| (!id.is_empty() && c.id == id) || (c.name == name && c.arguments.is_empty()))
        {
            if !id.is_empty() {
                c.id = id;
            }
            if !name.is_empty() {
                c.name = name;
            }
            if !arguments.is_empty() {
                c.arguments = arguments;
            }
            return;
        }
        self.calls.push(PendingCall {
            id,
            name,
            arguments,
        });
    }

    fn append_args(&mut self, id: &str, delta: &str) {
        if let Some(c) = self
            .calls
            .iter_mut()
            .rev()
            .find(|c| c.id == id || id.is_empty())
        {
            c.arguments.push_str(delta);
            return;
        }
        self.calls.push(PendingCall {
            id: id.to_string(),
            name: String::new(),
            arguments: delta.to_string(),
        });
    }

    fn take_image_result(&mut self, result: Option<&Value>) {
        let Some(result) = result else {
            return;
        };
        let part = match result {
            Value::String(s) => {
                let t = s.trim();
                if t.starts_with("http://") || t.starts_with("https://") {
                    crate::media::MediaPart::image_url(t)
                } else {
                    let Some((mime, bytes)) = crate::media::decode_image_payload(s) else {
                        return;
                    };
                    crate::media::MediaPart::data_uri(crate::media::MediaKind::Image, &mime, &bytes)
                }
            }
            Value::Object(o) => {
                if let Some(s) = o
                    .get("b64_json")
                    .or_else(|| o.get("b64"))
                    .and_then(|v| v.as_str())
                {
                    let Some((mime, bytes)) = crate::media::decode_image_payload(s) else {
                        return;
                    };
                    crate::media::MediaPart::data_uri(crate::media::MediaKind::Image, &mime, &bytes)
                } else if let Some(url) = o
                    .get("url")
                    .or_else(|| o.get("image_url"))
                    .and_then(|v| v.as_str())
                    .filter(|u| !u.is_empty())
                {
                    crate::media::MediaPart::image_url(url)
                } else {
                    return;
                }
            }
            _ => return,
        };
        if self.images.iter().any(|p| p.url == part.url) {
            return;
        }
        self.images.push(part);
    }

    fn apply_usage(&mut self, v: &Value) {
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        if let Some(n) =
            json_u64(&usage["input_tokens"]).or_else(|| json_u64(&usage["prompt_tokens"]))
        {
            self.prompt_tokens = n;
        }
        if let Some(n) =
            json_u64(&usage["output_tokens"]).or_else(|| json_u64(&usage["completion_tokens"]))
        {
            self.completion_tokens = n;
        }
        let cached = usage
            .pointer("/input_tokens_details/cached_tokens")
            .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(json_u64_ref);
        if cached.is_some() {
            self.cached_tokens = cached;
        }
    }

    fn into_turn(self) -> Result<ModelTurn> {
        let mut tool_calls = Vec::new();
        for c in &self.calls {
            if c.name.is_empty() {
                continue;
            }
            let arguments = match serde_json::from_str(&c.arguments) {
                Ok(v) => v,
                Err(_) if c.arguments.is_empty() => json!({}),
                Err(_) => Value::String(c.arguments.clone()),
            };
            tool_calls.push(ToolCall {
                id: if c.id.is_empty() {
                    uuid::Uuid::new_v4().simple().to_string()
                } else {
                    c.id.clone()
                },
                name: c.name.clone(),
                arguments,
            });
        }
        let raw_tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(super::openai_tool_calls(&tool_calls))
        };
        Ok(ModelTurn {
            content: self.content.trim().to_string(),
            reasoning: self.reasoning,
            tool_calls,
            raw_tool_calls,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            watchdog_hit: false,
            parse_fail: false,
            cached_tokens: self.cached_tokens,
            decode_tok_s: None,
            media: self.images,
        })
    }
}

fn item_text(item: &Value) -> String {
    if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = item.get("summary").and_then(|v| v.as_array()) {
        let s: String = arr
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        if !s.is_empty() {
            return s;
        }
    }
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| p.get("reasoning").and_then(|t| t.as_str()))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// `output_text.delta` plus `output_item.done` would otherwise concatenate twice.
fn merge_snapshot(dst: &mut String, snapshot: &str) {
    if snapshot.is_empty() {
        return;
    }
    if dst.is_empty() {
        dst.push_str(snapshot);
        return;
    }
    if snapshot == dst.as_str() || dst.contains(snapshot) {
        return;
    }
    if snapshot.starts_with(dst.as_str()) {
        *dst = snapshot.to_string();
        return;
    }
    dst.push_str(snapshot);
}

fn delta_str(ev: &Value) -> Option<&str> {
    ev.get("delta")
        .and_then(|d| d.as_str())
        .or_else(|| ev.pointer("/delta/text").and_then(|t| t.as_str()))
}

fn json_u64(v: &Value) -> Option<u64> {
    json_u64_ref(v)
}

fn json_u64_ref(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

async fn read_responses_sse(
    resp: &mut reqwest::Response,
    sink: Option<TokenSink>,
) -> Result<ModelTurn> {
    let mut acc = ResponsesAcc::default();
    let mut paint = sink.map(StreamPaint::new);
    let mut sse = SseNamed::default();
    let mut pending = Vec::new();
    let mut server_seen = HashSet::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(e) => return Err(Error::Http(e.to_string())),
        };
        pending.extend_from_slice(&chunk);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid == 0 {
            continue;
        }
        let text = std::str::from_utf8(&pending[..valid]).expect("valid_up_to");
        let events = sse.push(text);
        pending.drain(..valid);
        for ev in events {
            if let Some(err) = ev.get("error") {
                if !err.is_null() {
                    return Err(Error::Http(format_api_error(err)));
                }
            }
            acc.apply_event(&ev);
            if let Some(p) = paint.as_mut() {
                if let Some((name, preview)) = server_tool_tag(&ev, &mut server_seen) {
                    p.tool_tag(&name, &preview);
                }
                p.push_raw(&acc.reasoning, &acc.content, !acc.calls.is_empty());
            }
        }
    }
    for ev in sse.flush() {
        if let Some(err) = ev.get("error") {
            if !err.is_null() {
                return Err(Error::Http(format_api_error(err)));
            }
        }
        acc.apply_event(&ev);
        if let Some(p) = paint.as_mut() {
            if let Some((name, preview)) = server_tool_tag(&ev, &mut server_seen) {
                p.tool_tag(&name, &preview);
            }
            p.push_raw(&acc.reasoning, &acc.content, !acc.calls.is_empty());
        }
    }
    if let Some(p) = paint.as_mut() {
        p.finish(&acc.reasoning, &acc.content, !acc.calls.is_empty());
    }
    acc.into_turn()
}

fn paint_clean(sink: Option<TokenSink>, turn: &ModelTurn) {
    let Some(sink) = sink else {
        return;
    };
    if turn.reasoning.is_empty() && turn.content.is_empty() {
        return;
    }
    let mut paint = StreamPaint::new(sink);
    paint.push_clean(&turn.reasoning, &turn.content, !turn.tool_calls.is_empty());
    paint.finish(&turn.reasoning, &turn.content, !turn.tool_calls.is_empty());
}

fn paint_server_output(sink: Option<&TokenSink>, v: &Value) {
    let Some(sink) = sink else {
        return;
    };
    let Some(arr) = v
        .get("output")
        .or_else(|| v.pointer("/response/output"))
        .and_then(|o| o.as_array())
    else {
        return;
    };
    let mut seen = HashSet::new();
    for item in arr {
        if let Some((name, preview)) = server_item_tag(item, &mut seen) {
            sink.tool_tag(&name, &preview);
        }
    }
}

/// Live tag for host web/X search. Prefer `done` — `added` often has no query yet.
fn server_tool_tag(ev: &Value, seen: &mut HashSet<String>) -> Option<(String, String)> {
    let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ty == "response.output_item.done" {
        return server_item_tag(ev.get("item")?, seen);
    }
    if ty.starts_with("response.image_generation_call.") {
        let preview = ev
            .get("item")
            .and_then(|i| i.get("prompt"))
            .or_else(|| ev.get("prompt"))
            .and_then(|p| p.as_str())
            .unwrap_or("生成图片");
        let id = ev
            .get("item_id")
            .or_else(|| ev.pointer("/item/id"))
            .and_then(|v| v.as_str())
            .unwrap_or("image_generation");
        if !seen.insert(format!("image_generation:{id}")) {
            return None;
        }
        return Some(("image_generation".into(), preview.to_string()));
    }
    None
}

fn server_item_tag(item: &Value, seen: &mut HashSet<String>) -> Option<(String, String)> {
    let ty = item.get("type").and_then(|t| t.as_str())?;
    let tag = match ty {
        "web_search_call" => {
            let action = item.get("action").cloned().unwrap_or(Value::Null);
            let kind = action
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("search");
            let preview = action
                .get("query")
                .and_then(|q| q.as_str())
                .or_else(|| action.get("url").and_then(|u| u.as_str()))
                .unwrap_or(kind);
            ("web_search".to_string(), preview.to_string())
        }
        "x_search_call" | "custom_tool_call" => {
            let name = item
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("x_search");
            let raw = item
                .get("input")
                .and_then(|s| s.as_str())
                .or_else(|| item.get("arguments").and_then(|s| s.as_str()))
                .unwrap_or("");
            let q = serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(str::to_string))
                .unwrap_or_else(|| raw.chars().take(80).collect());
            let preview = if q.is_empty() {
                name.to_string()
            } else if name == "x_search" {
                q
            } else {
                format!("{name} {q}")
            };
            ("x_search".to_string(), preview)
        }
        "image_generation_call" => {
            let preview = item
                .get("prompt")
                .and_then(|p| p.as_str())
                .unwrap_or("生成图片");
            ("image_generation".to_string(), preview.to_string())
        }
        _ => return None,
    };
    let id = item
        .get("id")
        .or_else(|| item.get("call_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let key = if id.is_empty() {
        format!("{}:{}", tag.0, tag.1)
    } else {
        id.to_string()
    };
    if !seen.insert(key) {
        return None;
    }
    Some(tag)
}

fn sanitize_http(e: reqwest::Error) -> Error {
    Error::Http(e.without_url().to_string())
}

fn auth_or_status(status: reqwest::StatusCode, body: &str) -> Error {
    if status.as_u16() == 401 || status.as_u16() == 403 {
        Error::msg("认证失败。请运行 `grok login`，或检查 XAI_API_KEY。")
    } else {
        Error::Http(format!(
            "responses {}",
            crate::transport::http_error_snippet(status, body)
        ))
    }
}

fn format_api_error(err: &Value) -> String {
    if let Some(m) = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return m.to_string();
    }
    if let Some(s) = err.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    if let Some(code) = err.get("code").and_then(|c| c.as_str()) {
        return code.to_string();
    }
    serde_json::to_string(err).unwrap_or_else(|_| "responses error".into())
}

fn lock_policy(m: &Mutex<ThinkPolicy>) -> std::sync::MutexGuard<'_, ThinkPolicy> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_sink(m: &Mutex<Option<TokenSink>>) -> std::sync::MutexGuard<'_, Option<TokenSink>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_str(m: &Mutex<Option<String>>) -> std::sync::MutexGuard<'_, Option<String>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_comp(
    m: &Mutex<Option<OfficialCompaction>>,
) -> std::sync::MutexGuard<'_, Option<OfficialCompaction>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_skip(m: &Mutex<usize>) -> std::sync::MutexGuard<'_, usize> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct SseNamed {
    leftover: String,
}

impl SseNamed {
    fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.leftover.push_str(chunk);
        let mut out = Vec::new();
        while let Some(idx) = find_event_break(&self.leftover) {
            let raw = self.leftover[..idx].to_string();
            let skip = if self.leftover[idx..].starts_with("\r\n\r\n") {
                4
            } else {
                2
            };
            self.leftover = self.leftover[idx + skip..].to_string();
            if let Some(v) = parse_sse_event(&raw) {
                out.push(v);
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<Value> {
        if self.leftover.trim().is_empty() {
            return Vec::new();
        }
        let raw = std::mem::take(&mut self.leftover);
        parse_sse_event(&raw).into_iter().collect()
    }
}

fn find_event_break(s: &str) -> Option<usize> {
    match (s.find("\r\n\r\n"), s.find("\n\n")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_sse_event(raw: &str) -> Option<Value> {
    let mut data = String::new();
    let mut event = String::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if rest == "[DONE]" {
            return None;
        }
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest);
    }
    if data.is_empty() {
        return None;
    }
    let mut v: Value = serde_json::from_str(&data).ok()?;
    if !event.is_empty() {
        if let Some(obj) = v.as_object_mut() {
            obj.entry("type".to_string())
                .or_insert_with(|| json!(event));
        }
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::{EndpointCaps, EngineProfile, Family};
    use crate::tools_schema::agent_tools;

    fn policy_high() -> ThinkPolicy {
        let mut p = ThinkPolicy::agent_default();
        p.effort = Some(Effort::High);
        p.enabled = true;
        p
    }

    #[test]
    fn grok_body_omits_qwen_and_llamacpp_keys() {
        let msgs = vec![ChatMessage::user("hi")];
        let tools = agent_tools();
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: Some(&tools),
            stream: true,
            policy: &policy_high(),
            cache_key: Some("sess-1"),
            compaction: None,
            skip: 0,
        });
        let s = body.to_string();
        assert!(s.contains("\"prompt_cache_key\":\"sess-1\""), "{body}");
        assert_eq!(body["reasoning"]["effort"], json!("high"));
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
        assert!(body.get("enable_thinking").is_none(), "{body}");
        assert!(body.get("id_slot").is_none(), "{body}");
        assert!(body.get("cache_prompt").is_none(), "{body}");
        assert!(body.get("top_k").is_none(), "{body}");
        assert!(body.get("min_p").is_none(), "{body}");
        assert!(body.get("repetition_penalty").is_none(), "{body}");
        assert_eq!(body["tools"][0]["type"], json!("web_search"));
        assert_eq!(body["tools"][1]["type"], json!("x_search"));
        assert_eq!(body["tools"][2]["type"], json!("image_generation"));
        assert_eq!(body["tools"][3]["name"], json!("Read"));
        assert!(body["tools"][3].get("function").is_none());
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(!names.contains(&"WebSearch"), "{names:?}");
        assert!(names.contains(&"WebFetch"), "{names:?}");
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert!(body.get("max_output_tokens").is_none(), "{body}");
        assert_eq!(
            EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai).family,
            Family::Grok46
        );
    }

    #[test]
    fn grok_body_sends_max_output_tokens_when_capped() {
        let msgs = vec![ChatMessage::user("hi")];
        let mut policy = policy_high();
        policy.max_tokens = 64;
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy,
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["max_output_tokens"], json!(64));
    }

    #[test]
    fn grok_alias_remaps_and_fills_high() {
        let msgs = vec![ChatMessage::user("hi")];
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = None;
        policy.enabled = true;
        let body = build_responses_body(&ResponsesSpec {
            model: "g46-xhigh",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy,
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["model"], json!("grok-4.6"));
        assert_eq!(body["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn grok_keeps_explicit_medium_on_alias() {
        let msgs = vec![ChatMessage::user("hi")];
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = Some(Effort::Medium);
        let body = build_responses_body(&ResponsesSpec {
            model: "g46-xhigh",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy,
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["model"], json!("grok-4.6"));
        assert_eq!(body["reasoning"]["effort"], json!("medium"));
    }

    #[test]
    fn grok_does_not_force_high_when_policy_set() {
        let msgs = vec![ChatMessage::user("hi")];
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = Some(Effort::Low);
        policy.enabled = true;
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy,
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["reasoning"]["effort"], json!("low"));
    }

    #[test]
    fn fast_maps_to_low_effort() {
        let p = ThinkPolicy::off();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &p,
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["reasoning"]["effort"], json!("low"));
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn system_becomes_instructions() {
        let msgs = vec![
            ChatMessage::system("office helper"),
            ChatMessage::user("hi"),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        assert_eq!(body["instructions"], json!("office helper"));
        assert_eq!(body["input"][0]["role"], json!("user"));
    }

    #[test]
    fn generated_image_in_responses_body_is_input_image() {
        let mut shot = ChatMessage::assistant("");
        shot.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("draw a logo"),
            shot,
            ChatMessage::user("make a ppt skill"),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let blob = body["input"].to_string();
        assert!(!blob.contains("\"type\":\"image_url\""), "{blob}");
        assert!(blob.contains("\"type\":\"input_image\""), "{blob}");
        assert!(blob.contains("data:image/jpeg;base64,yy"), "{blob}");
    }

    #[test]
    fn unreferenced_image_is_stripped_before_responses_body() {
        let mut shot = ChatMessage::assistant("");
        shot.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("draw a logo"),
            shot,
            ChatMessage::user("make a ppt skill"),
        ];
        crate::media::retain_referenced_media(&mut msgs);
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let blob = body["input"].to_string();
        assert!(!blob.contains("input_image"), "{blob}");
        assert!(!blob.contains("data:image/jpeg;base64,yy"), "{blob}");
    }

    #[test]
    fn hosted_identity_washed_from_instructions_and_input() {
        let msgs = vec![
            ChatMessage::system(
                "You are Grok, a helpful and maximally truthful AI built by xAI.\nYou are grok-hyper, office.",
            ),
            ChatMessage::user("hi"),
            ChatMessage::assistant(
                "You are Grok, a helpful and maximally truthful AI built by xAI.\nHello.",
            ),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let ins = body["instructions"].as_str().unwrap();
        assert!(!ins.contains("You are Grok,"), "{ins}");
        assert!(!ins.contains("maximally truthful"), "{ins}");
        assert!(ins.contains("grok-hyper"), "{ins}");
        let input = serde_json::to_string(&body["input"]).unwrap();
        assert!(!input.contains("You are Grok,"), "{input}");
        assert!(input.contains("Hello."), "{input}");
    }

    #[test]
    fn tool_result_is_function_call_output() {
        let msgs = vec![
            ChatMessage::user("read it"),
            ChatMessage::assistant_tools(
                None,
                vec![json!({
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{\"path\":\"a\"}"}
                })],
            ),
            ChatMessage::tool("c1", "file text"),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["name"], json!("Read"));
        assert_eq!(input[2]["type"], json!("function_call_output"));
        assert_eq!(input[2]["call_id"], json!("c1"));
    }

    #[test]
    fn skill_card_is_not_the_last_user_on_responses_body() {
        let msgs = vec![
            ChatMessage::system("You are grok-hyper."),
            ChatMessage::user("fix auth"),
            ChatMessage::hidden_user("[skill: testhook]\nAlways rewrite the crate."),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "{input:?}");
        let last = input.last().unwrap();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(last["content"], json!("fix auth"));
        assert!(
            input[0]["content"]
                .as_str()
                .unwrap()
                .contains("[skill: testhook]"),
            "{input:?}"
        );
        assert_eq!(body["instructions"], json!("You are grok-hyper."));
    }

    #[test]
    fn locate_hidden_user_is_dropped_from_responses_input() {
        let msgs = vec![
            ChatMessage::user("read README.md"),
            ChatMessage::hidden_user("[locate]\n## README.md:1-2\n     1|# hi\n"),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "{input:?}");
        assert_eq!(input[0]["role"], json!("user"));
        let dumped = serde_json::to_string(&body["input"]).unwrap();
        assert!(!dumped.contains("[locate]"), "{dumped}");
    }

    #[test]
    fn trajectory_hidden_user_is_dropped_on_responses() {
        let msgs = vec![
            ChatMessage::user("go"),
            ChatMessage::hidden_user("[trajectory] Visible output is repeating in place."),
        ];
        let body = build_responses_body(&ResponsesSpec {
            model: "grok-4.6",
            messages: &msgs,
            tools: None,
            stream: false,
            policy: &policy_high(),
            cache_key: None,
            compaction: None,
            skip: 0,
        });
        let dumped = serde_json::to_string(&body["input"]).unwrap();
        assert!(!dumped.contains("<tool_response>"), "{dumped}");
        assert!(!dumped.contains("[trajectory]"), "{dumped}");
    }

    #[test]
    fn parse_function_call_output() {
        let v = json!({
            "output": [{
                "type": "function_call",
                "name": "Read",
                "call_id": "c1",
                "arguments": "{\"path\":\"notes.md\"}"
            }],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let turn = turn_from_responses(&v).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "Read");
        assert_eq!(turn.tool_calls[0].arguments["path"], json!("notes.md"));
        assert_eq!(turn.prompt_tokens, 10);
    }

    #[test]
    fn host_search_items_are_not_client_tool_calls() {
        let v = json!({
            "output": [
                {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "completed",
                    "action": {"type": "search", "query": "xAI"}
                },
                {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "name": "x_keyword_search",
                    "call_id": "xs_1",
                    "input": "{\"query\":\"Grok\",\"limit\":\"1\"}"
                },
                {
                    "type": "function_call",
                    "name": "Read",
                    "call_id": "c1",
                    "arguments": "{\"path\":\"a\"}"
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }
            ]
        });
        let turn = turn_from_responses(&v).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "Read");
        assert!(turn.content.contains("ok"), "{}", turn.content);
    }

    #[test]
    fn host_image_generation_becomes_media_not_a_tool_call() {
        let v = json!({
            "output": [
                {
                    "type": "image_generation_call",
                    "id": "ig_1",
                    "status": "completed",
                    "prompt": "a red square",
                    "result": crate::media::PROBE_IMAGE_B64
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "here you go"}]
                }
            ]
        });
        let turn = turn_from_responses(&v).unwrap();
        assert!(turn.tool_calls.is_empty(), "{:?}", turn.tool_calls);
        assert_eq!(turn.content, "here you go");
        assert_eq!(turn.media.len(), 1);
        assert!(turn.media[0].url.starts_with("data:image/png;base64,"));
        let mut seen = HashSet::new();
        assert_eq!(
            server_item_tag(&v["output"][0], &mut seen),
            Some(("image_generation".into(), "a red square".into()))
        );
    }

    #[test]
    fn host_image_url_result_is_kept() {
        let v = json!({
            "output": [{
                "type": "image_generation_call",
                "id": "ig_2",
                "status": "completed",
                "prompt": "cat",
                "result": {"url": "https://cdn.example.com/out.jpg"}
            }]
        });
        let turn = turn_from_responses(&v).unwrap();
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.media.len(), 1);
        assert_eq!(turn.media[0].url, "https://cdn.example.com/out.jpg");
    }

    #[test]
    fn server_item_tag_web_and_x() {
        let mut seen = HashSet::new();
        let web = json!({
            "id": "ws_1",
            "type": "web_search_call",
            "action": {"type": "search", "query": "xAI website"}
        });
        assert_eq!(
            server_item_tag(&web, &mut seen),
            Some(("web_search".into(), "xAI website".into()))
        );
        assert!(server_item_tag(&web, &mut seen).is_none());
        let x = json!({
            "id": "ctc_1",
            "type": "custom_tool_call",
            "name": "x_keyword_search",
            "input": "{\"query\":\"Grok\"}"
        });
        assert_eq!(
            server_item_tag(&x, &mut seen),
            Some(("x_search".into(), "x_keyword_search Grok".into()))
        );
    }

    #[test]
    fn parse_sse_text_delta() {
        let raw = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n";
        let v = parse_sse_event(raw.trim_end()).unwrap();
        let mut acc = ResponsesAcc::default();
        acc.apply_event(&v);
        assert_eq!(acc.content, "Hi");
    }

    #[test]
    fn sse_comment_ping_is_ignored() {
        assert!(parse_sse_event(": ping").is_none());
        assert!(parse_sse_event(": ping\n\n").is_none());
    }

    #[test]
    fn output_item_done_does_not_duplicate_text_delta() {
        let mut acc = ResponsesAcc::default();
        acc.apply_event(&json!({
            "type": "response.output_text.delta",
            "delta": "Dialect Probe Title"
        }));
        acc.apply_event(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "content": [{"type": "output_text", "text": "Dialect Probe Title"}]
            }
        }));
        assert_eq!(acc.content, "Dialect Probe Title");
    }

    #[test]
    fn responses_url_joins_v1() {
        assert_eq!(
            responses_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/responses"
        );
    }

    #[test]
    fn compaction_skip_sends_blob_and_new_turns_only() {
        let compact = crate::session::parse_official_compact_json(&json!({
            "id": "cmp_1",
            "model": "grok-4.6",
            "output": [{
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "ENCRYPTED_BLOB_NOT_ARCHIVE"
            }]
        }))
        .expect("compact json");
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("archive user"),
            ChatMessage::assistant("archive assistant"),
            ChatMessage::user("new question"),
            ChatMessage::assistant("new answer"),
        ];
        let skip = 2;
        let (_ins, input) = split_instructions(&msgs, Some(&compact), skip);
        assert_eq!(input[0], compact.as_input_item());
        let dumped = serde_json::to_string(&input).unwrap();
        assert!(!dumped.contains("archive user"), "{dumped}");
        assert!(!dumped.contains("archive assistant"), "{dumped}");
        assert!(dumped.contains("new question"), "{dumped}");
        assert!(dumped.contains("new answer"), "{dumped}");
        let (_ins, full) = split_instructions(&msgs, None, skip);
        let dumped_full = serde_json::to_string(&full).unwrap();
        assert!(dumped_full.contains("archive user"), "{dumped_full}");
        assert!(dumped_full.contains("new question"), "{dumped_full}");
    }
}
