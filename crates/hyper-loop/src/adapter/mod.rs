//! OpenAI-compat request builders. Omitted keys are absent; JSON `null` is never emitted.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::family::{EndpointCaps, EngineProfile, Family};
use crate::policy::{grok_forwarding_effort, Sampling, TemplateKwargs, ThinkPolicy};
use crate::template::ChatMessage;

#[derive(Clone, Debug)]
pub struct ChatRequestSpec<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [Value]>,
    pub stream: bool,
    pub policy: &'a ThinkPolicy,
    pub caps: &'a EndpointCaps,
    /// llama.cpp slot pin. Stable per session so a tiny probe cannot wipe a long prefix.
    pub id_slot: Option<i64>,
    /// llama.cpp `cache_prompt`. Default on for that profile.
    pub cache_prompt: bool,
    /// Lossy overlay: raise `repetition_penalty` to 1.1. Off keeps official 1.0.
    pub lossy_repeat: bool,
}

pub fn build_chat_body(spec: &ChatRequestSpec<'_>) -> Value {
    let profile = spec.caps.profile;
    let kwargs = spec.policy.template_kwargs(spec.caps);
    let mut sampling = spec.policy.sampling();
    if spec.lossy_repeat {
        sampling = sampling.with_lossy_repeat();
    }
    let mut root = Map::new();

    insert(
        &mut root,
        "model",
        Value::String(Family::wire_model_id(spec.model).to_string()),
    );
    let mut msgs: Vec<Value> = spec
        .messages
        .iter()
        .map(ChatMessage::to_api_value)
        .collect();
    // Qwen/Unsloth Jinja wants `function.arguments` as objects. Cursor and
    // xAI Chat Completions require a JSON string; a map 422s hop-2.
    if spec.caps.family == crate::family::Family::Grok46 || profile == EngineProfile::Xai {
        stringify_openai_tool_arguments(&mut msgs);
    }
    insert(&mut root, "messages", Value::Array(msgs));
    insert(&mut root, "stream", Value::Bool(spec.stream));
    if spec.stream {
        // OpenAI-compat SSE omits `usage` unless this is set. llama.cpp still
        // sends `timings` on the last chunk; include_usage also forwards
        // `prompt_tokens` / `prompt_tokens_details.cached_tokens`.
        insert(&mut root, "stream_options", json!({"include_usage": true}));
    }
    if spec.policy.max_tokens > 0 {
        insert(&mut root, "max_tokens", json!(spec.policy.max_tokens));
    }

    if spec.caps.family == crate::family::Family::Grok46 || profile == EngineProfile::Xai {
        let effort = grok_forwarding_effort(spec.policy, spec.model);
        insert(&mut root, "reasoning", json!({"effort": effort}));
        // grok-proxy Chat Completions reads `reasoning_effort`; Responses reads `reasoning`.
        insert(&mut root, "reasoning_effort", json!(effort));
        // Grok 4.6: thinking cannot be off. Never emit Qwen/llama.cpp keys.
    } else {
        match profile {
            EngineProfile::LlamaCpp | EngineProfile::Auto => {
                insert_openai_sampling(&mut root, &sampling);
                insert_local_sampling(&mut root, &sampling);
                insert_kwargs_object(&mut root, "chat_template_kwargs", &kwargs);
                if spec.cache_prompt {
                    insert(&mut root, "cache_prompt", json!(true));
                }
                if let Some(slot) = spec.id_slot {
                    insert(&mut root, "id_slot", json!(slot));
                }
            }
            EngineProfile::Vllm | EngineProfile::Sglang => {
                insert_openai_sampling(&mut root, &sampling);
                let mut extra = Map::new();
                insert_local_sampling(&mut extra, &sampling);
                insert_kwargs_object(&mut extra, "chat_template_kwargs", &kwargs);
                if !extra.is_empty() {
                    insert(&mut root, "extra_body", Value::Object(extra));
                }
            }
            EngineProfile::Generic => {
                insert_openai_sampling(&mut root, &sampling);
                let mut extra = Map::new();
                insert_kwargs_object(&mut extra, "chat_template_kwargs", &kwargs);
                if !extra.is_empty() {
                    insert(&mut root, "extra_body", Value::Object(extra));
                }
            }
            EngineProfile::Xai => {}
        }
    }

    if let Some(tools) = spec.tools {
        if !tools.is_empty() {
            insert(&mut root, "tools", Value::Array(tools.to_vec()));
        }
    }

    debug_assert!(!contains_null(&Value::Object(root.clone())));
    Value::Object(root)
}

fn insert(map: &mut Map<String, Value>, key: &str, value: Value) {
    debug_assert!(!value.is_null(), "refusing to insert null for {key}");
    map.insert(key.to_string(), value);
}

fn insert_openai_sampling(map: &mut Map<String, Value>, s: &Sampling) {
    insert(map, "temperature", json!(s.temperature));
    insert(map, "top_p", json!(s.top_p));
    insert(map, "presence_penalty", json!(s.presence_penalty));
}

fn insert_local_sampling(map: &mut Map<String, Value>, s: &Sampling) {
    insert(map, "top_k", json!(s.top_k));
    insert(map, "min_p", json!(s.min_p));
    insert(map, "repetition_penalty", json!(s.repetition_penalty));
}

fn stringify_openai_tool_arguments(messages: &mut [Value]) {
    for msg in messages {
        let Some(calls) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for call in calls {
            let Some(args) = call.pointer_mut("/function/arguments") else {
                continue;
            };
            if args.is_string() {
                continue;
            }
            *args = Value::String(args.to_string());
        }
    }
}

fn insert_kwargs_object(map: &mut Map<String, Value>, key: &str, kwargs: &TemplateKwargs) {
    let obj = kwargs.to_json_object();
    if !obj.is_empty() {
        insert(map, key, Value::Object(obj));
    }
}

pub fn contains_null(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.iter().any(contains_null),
        Value::Object(m) => m.values().any(contains_null),
        _ => false,
    }
}

pub fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if let Some(found) = m.get(key) {
                return Some(found);
            }
            m.values().find_map(|child| find_key(child, key))
        }
        Value::Array(a) => a.iter().find_map(|child| find_key(child, key)),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessageDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::{EndpointCaps, EngineProfile, Family};
    use crate::policy::ThinkPolicy;
    use crate::template::ChatMessage;
    use crate::tools_schema::agent_tools;

    fn spec<'a>(
        caps: &'a EndpointCaps,
        policy: &'a ThinkPolicy,
        msgs: &'a [ChatMessage],
        tools: Option<&'a [Value]>,
    ) -> ChatRequestSpec<'a> {
        ChatRequestSpec {
            model: "Qwen3.8-27B-UD-Q8",
            messages: msgs,
            tools,
            stream: false,
            policy,
            caps,
            id_slot: None,
            cache_prompt: false,
            lossy_repeat: false,
        }
    }

    #[test]
    fn stream_requests_include_usage() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.stream = true;
        let body = build_chat_body(&s);
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert!(!contains_null(&body), "{body}");

        let off = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert_eq!(off["stream"], json!(false));
        assert!(off.get("stream_options").is_none());
    }

    #[test]
    fn llamacpp_pins_slot_and_cache_prompt() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.cache_prompt = true;
        s.id_slot = Some(2);
        let body = build_chat_body(&s);
        assert_eq!(body["cache_prompt"], json!(true));
        assert_eq!(body["id_slot"], json!(2));
        assert!(!contains_null(&body), "{body}");
        let generic = EndpointCaps::for_family(Family::Qwen38, EngineProfile::Generic);
        let g = spec(&generic, &policy, &msgs, None);
        let body = build_chat_body(&g);
        assert!(body.get("cache_prompt").is_none());
        assert!(body.get("id_slot").is_none());
    }

    #[test]
    fn llamacpp_kwargs_at_root_no_null() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert!(!contains_null(&body), "{body}");
        assert_eq!(
            body["chat_template_kwargs"]["reasoning_effort"],
            json!("low")
        );
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
        assert_eq!(
            body["chat_template_kwargs"]["preserve_thinking"],
            json!(true)
        );
        assert!(body.get("extra_body").is_none());
        assert_eq!(body["temperature"], json!(1.0));
        assert_eq!(body["top_p"], json!(0.95));
        assert_eq!(body["top_k"], json!(20));
        assert_eq!(body["repetition_penalty"], json!(1.0));
    }

    #[test]
    fn lossy_repeat_raises_penalty() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.lossy_repeat = true;
        let body = build_chat_body(&s);
        assert_eq!(body["repetition_penalty"], json!(1.1));
        let off = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert_eq!(off["repetition_penalty"], json!(1.0));
    }

    #[test]
    fn off_omits_reasoning_effort() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::off();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert!(!contains_null(&body), "{body}");
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        assert_eq!(
            body["chat_template_kwargs"]["preserve_thinking"],
            json!(true)
        );
        assert!(find_key(&body, "reasoning_effort").is_none(), "{body}");
        assert_eq!(body["temperature"], json!(0.7));
        assert_eq!(body["top_p"], json!(0.8));
        assert_eq!(body["presence_penalty"], json!(1.5));
    }

    #[test]
    fn vllm_puts_kwargs_in_extra_body() {
        let mut caps = EndpointCaps::qwen38_llamacpp();
        caps.profile = EngineProfile::Vllm;
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(
            body["extra_body"]["chat_template_kwargs"]["reasoning_effort"],
            json!("low")
        );
        assert_eq!(body["extra_body"]["top_k"], json!(20));
        assert!(body.get("top_k").is_none());
        assert!(body.get("cache_prompt").is_none());
        assert!(body.get("id_slot").is_none());
    }

    #[test]
    fn vllm_off_sends_preserve_in_extra_body() {
        let mut caps = EndpointCaps::qwen38_llamacpp();
        caps.profile = EngineProfile::Vllm;
        let policy = ThinkPolicy::off();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert_eq!(
            body["extra_body"]["chat_template_kwargs"]["preserve_thinking"],
            json!(true)
        );
        assert_eq!(
            body["extra_body"]["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        assert!(body.get("cache_prompt").is_none());
        assert!(body.get("id_slot").is_none());
    }

    #[test]
    fn sglang_matches_vllm_shape() {
        let mut caps = EndpointCaps::qwen38_llamacpp();
        caps.profile = EngineProfile::Sglang;
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert!(body["extra_body"]["chat_template_kwargs"].is_object());
    }

    #[test]
    fn generic_openai_plus_extra_kwargs() {
        let caps = EndpointCaps::for_family(Family::Qwen38, EngineProfile::Generic);
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert!(body.get("top_k").is_none());
        assert_eq!(
            body["extra_body"]["chat_template_kwargs"]["reasoning_effort"],
            json!("low")
        );
    }

    #[test]
    fn qwen35_never_sends_xhigh_or_effort() {
        let caps = EndpointCaps::for_family(Family::Qwen35, EngineProfile::LlamaCpp);
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![ChatMessage::user("hi")];
        let tools = agent_tools();
        let body = build_chat_body(&spec(&caps, &policy, &msgs, Some(&tools)));
        assert!(find_key(&body, "reasoning_effort").is_none(), "{body}");
        assert_eq!(body["tools"].as_array().unwrap().len(), 17);
        assert!(!contains_null(&body));
    }

    #[test]
    fn image_parts_serialize_as_content_array() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let mut msg = ChatMessage::user("what color");
        msg.parts = vec![crate::media::MediaPart::image_url(
            "data:image/png;base64,xx",
        )];
        let msgs = vec![msg];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "{content}");
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,xx");
        assert_eq!(content[1]["type"], "text");
        assert!(!contains_null(&body));
    }

    #[test]
    fn grok46_omits_qwen_and_llamacpp_keys() {
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai);
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = Some(crate::policy::Effort::High);
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.id_slot = Some(2);
        s.cache_prompt = true;
        let body = build_chat_body(&s);
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
        assert!(find_key(&body, "enable_thinking").is_none(), "{body}");
        assert!(body.get("id_slot").is_none(), "{body}");
        assert!(body.get("cache_prompt").is_none(), "{body}");
        assert!(body.get("top_k").is_none(), "{body}");
        assert!(body.get("min_p").is_none(), "{body}");
        assert!(body.get("repetition_penalty").is_none(), "{body}");
        assert_eq!(body["reasoning"]["effort"], json!("high"));
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert!(!contains_null(&body), "{body}");
    }

    #[test]
    fn grok46_alias_remaps_and_fills_xhigh() {
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai);
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = None;
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.model = "g46-xhigh";
        let body = build_chat_body(&s);
        assert_eq!(body["model"], json!("grok-4.6"));
        assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
        assert_eq!(body["reasoning_effort"], json!("xhigh"));
    }

    #[test]
    fn grok46_keeps_explicit_medium_on_alias() {
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai);
        let mut policy = ThinkPolicy::agent_default();
        policy.effort = Some(crate::policy::Effort::Medium);
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = spec(&caps, &policy, &msgs, None);
        s.model = "g46-xhigh";
        let body = build_chat_body(&s);
        assert_eq!(body["model"], json!("grok-4.6"));
        assert_eq!(body["reasoning"]["effort"], json!("medium"));
        assert_eq!(body["reasoning_effort"], json!("medium"));
    }

    #[test]
    fn grok46_sends_max_tokens_when_capped() {
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Xai);
        let mut policy = ThinkPolicy::agent_default();
        policy.max_tokens = 64;
        let msgs = vec![ChatMessage::user("hi")];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        assert_eq!(body["max_tokens"], json!(64));
    }

    fn read_call_with_object_args() -> ChatMessage {
        ChatMessage::assistant_tools(
            None,
            vec![json!({
                "id": "call-1",
                "type": "function",
                "function": {
                    "name": "Read",
                    "arguments": {"path": "README.md"}
                }
            })],
        )
    }

    #[test]
    fn grok46_chat_wire_stringifies_tool_arguments() {
        let caps = EndpointCaps::for_family(Family::Grok46, EngineProfile::Generic);
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![
            ChatMessage::user("read it"),
            read_call_with_object_args(),
            ChatMessage::tool("call-1", "ok"),
        ];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        let args = &body["messages"][1]["tool_calls"][0]["function"]["arguments"];
        assert!(
            args.is_string(),
            "grok hop-2 must send a string, got {args}"
        );
        assert_eq!(args.as_str().unwrap(), "{\"path\":\"README.md\"}");
    }

    #[test]
    fn qwen_llamacpp_keeps_tool_argument_objects() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let policy = ThinkPolicy::agent_default();
        let msgs = vec![
            ChatMessage::user("read it"),
            read_call_with_object_args(),
            ChatMessage::tool("call-1", "ok"),
        ];
        let body = build_chat_body(&spec(&caps, &policy, &msgs, None));
        let args = &body["messages"][1]["tool_calls"][0]["function"]["arguments"];
        assert!(
            args.is_object(),
            "Unsloth hop-2 wants an object, got {args}"
        );
        assert_eq!(args["path"], json!("README.md"));
    }
}
