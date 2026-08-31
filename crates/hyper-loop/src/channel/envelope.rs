//! QwenPaw native inbound payload: platform message → `content_parts`.
//!
//! Adapters (Telegram, webhook, …) convert their native JSON into this shape.
//! The agent never sees DingTalk/Telegram wire types.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::media::{MediaKind, MediaPart};
use crate::template::ChatMessage;

/// QwenPaw `ChannelAddress` — where the reply goes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAddress {
    /// `dm` | `channel` | `webhook` | `console`
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl ChannelAddress {
    pub fn handle(&self) -> String {
        if let Some(Value::String(h)) = self.extra.get("to_handle") {
            return h.clone();
        }
        if self.kind.is_empty() {
            return self.id.clone();
        }
        format!("{}:{}", self.kind, self.id)
    }
}

/// One content block. Wire names match QwenPaw `TextContent` / `ImageContent`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentPart {
    Text {
        #[serde(default)]
        text: String,
    },
    Image {
        #[serde(default)]
        image_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    Video {
        #[serde(default)]
        video_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    Audio {
        #[serde(default)]
        audio_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    File {
        #[serde(default)]
        file_url: String,
        #[serde(default)]
        file_id: String,
        #[serde(default)]
        name: String,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }

    fn image_src(&self) -> Option<&str> {
        match self {
            Self::Image { image_url, url, .. } => nonempty(image_url).or_else(|| nonempty(url)),
            _ => None,
        }
    }

    pub fn media_part(&self) -> Option<MediaPart> {
        match self {
            Self::Image { mime, .. } => {
                let url = self.image_src()?.to_string();
                let mut p = MediaPart::image_url(url);
                if !mime.is_empty() {
                    p.mime = mime.clone();
                }
                Some(p)
            }
            Self::Video {
                video_url,
                url,
                mime,
                ..
            } => {
                let src = nonempty(video_url).or_else(|| nonempty(url))?;
                let mut p = MediaPart::video_url(src);
                if !mime.is_empty() {
                    p.mime = mime.clone();
                }
                Some(p)
            }
            Self::Audio {
                audio_url,
                url,
                mime,
                ..
            } => {
                let src = nonempty(audio_url).or_else(|| nonempty(url))?;
                let m = if mime.is_empty() {
                    "audio/wav"
                } else {
                    mime.as_str()
                };
                Some(MediaPart::audio_url(src, m))
            }
            Self::File { file_url, name, .. } => {
                let src = nonempty(file_url)?;
                let lower = name.to_ascii_lowercase();
                if lower.ends_with(".mp4") || lower.ends_with(".webm") {
                    Some(MediaPart::video_url(src))
                } else if MediaKind::parse(
                    std::path::Path::new(name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or(""),
                ) == Some(MediaKind::Image)
                    || lower.ends_with(".png")
                    || lower.ends_with(".jpg")
                    || lower.ends_with(".jpeg")
                    || lower.ends_with(".webp")
                    || lower.ends_with(".gif")
                {
                    Some(MediaPart::image_url(src))
                } else {
                    None
                }
            }
            Self::Text { .. } => None,
        }
    }

    pub fn fallback_line(&self) -> Option<String> {
        match self {
            Self::Text { .. } => None,
            Self::Image { .. } => {
                let u = self.image_src().unwrap_or("");
                if u.starts_with("data:") || u.len() > 180 {
                    Some("[图片]".into())
                } else if u.is_empty() {
                    Some("[图片]".into())
                } else {
                    Some(format!("[Image: {u}]"))
                }
            }
            Self::Video { video_url, url, .. } => {
                let u = nonempty(video_url).or_else(|| nonempty(url)).unwrap_or("");
                if u.starts_with("data:") || u.is_empty() {
                    Some("[视频]".into())
                } else {
                    Some(format!("[Video: {u}]"))
                }
            }
            Self::Audio { .. } => Some("[语音]".into()),
            Self::File {
                file_url,
                file_id,
                name,
                ..
            } => {
                let label = if !name.is_empty() {
                    name.as_str()
                } else if !file_id.is_empty() {
                    file_id.as_str()
                } else {
                    nonempty(file_url).unwrap_or("file")
                };
                if file_url.starts_with("data:") {
                    Some(format!("[File: {label}]"))
                } else if !file_url.trim().is_empty() {
                    Some(format!("[File: {label}] ({file_url})"))
                } else {
                    Some(format!("[File: {label}]"))
                }
            }
        }
    }
}

/// QwenPaw native dict that hits `BaseChannel.consume_one`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NativePayload {
    #[serde(default, alias = "channel_id")]
    pub channel: String,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: String,
    #[serde(default, alias = "session")]
    pub session_id: String,
    #[serde(default)]
    pub content_parts: Vec<ContentPart>,
    /// Convenience when the adapter only has a string (old `channel.inbound`).
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub meta: Map<String, Value>,
}

impl NativePayload {
    pub fn text_only(channel: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            channel: channel.into(),
            content_parts: vec![ContentPart::text(&text)],
            text: text.clone(),
            ..Self::default()
        }
    }

    /// Same route as `self`, text-only. Leftover IM steer after the last
    /// tool hop becomes the next session_worker turn (sidecar `push_queue`).
    pub fn follow_up_text(&self, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            channel: self.channel.clone(),
            sender_id: self.sender_id.clone(),
            sender_name: self.sender_name.clone(),
            session_id: self.session_id.clone(),
            content_parts: vec![ContentPart::text(&text)],
            text: text.clone(),
            prompt: text,
            meta: self.meta.clone(),
        }
    }

    pub fn parts(&self) -> Vec<ContentPart> {
        if !self.content_parts.is_empty() {
            return self.content_parts.clone();
        }
        let t = if !self.text.is_empty() {
            &self.text
        } else {
            &self.prompt
        };
        if t.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(t)]
        }
    }

    pub fn query_text(&self) -> String {
        let mut out = String::new();
        for p in self.parts() {
            if let Some(t) = p.as_text() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        if out.is_empty() {
            for p in self.parts() {
                if let Some(line) = p.fallback_line() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&line);
                }
            }
        }
        out
    }

    pub fn media_parts(&self) -> Vec<MediaPart> {
        self.parts()
            .iter()
            .filter_map(ContentPart::media_part)
            .collect()
    }

    pub fn to_chat_message(&self) -> ChatMessage {
        let mut text = self.query_text();
        if text.trim().is_empty() {
            text = " ".into();
        } else if self.is_group() {
            // Hermes `[nickname|user_id]`: a shared group transcript is
            // meaningless to the model unless each line says who spoke.
            let who = if !self.sender_name.is_empty() {
                &self.sender_name
            } else {
                &self.sender_id
            };
            if !who.is_empty() {
                text = format!("[{who}] {text}");
            }
        }
        let mut msg = ChatMessage::user(text);
        msg.parts = self.media_parts();
        msg
    }

    pub fn is_group(&self) -> bool {
        bool_meta(&self.meta, "is_group")
    }

    pub fn is_mentioned(&self) -> bool {
        bool_meta(&self.meta, "is_mentioned") || bool_meta(&self.meta, "is_reply_to_bot")
    }

    /// Feishu card button / Telegram callback. Not a typed user message.
    pub fn is_choice_click(&self) -> bool {
        bool_meta(&self.meta, "choice_click")
            || string_meta(&self.meta, "callback_query_id").is_some_and(|id| !id.is_empty())
    }

    pub fn mark_choice_click(&mut self) {
        self.meta
            .insert("choice_click".into(), serde_json::json!(true));
    }

    pub fn chat_id(&self) -> String {
        string_meta(&self.meta, "chat_id")
            .or_else(|| string_meta(&self.meta, "conversation_id"))
            .unwrap_or_else(|| self.sender_id.clone())
    }

    /// One inbound user message → one progress bubble (Telegram/Feishu edit).
    pub fn progress_bubble_key(&self) -> String {
        let chat = self.chat_id();
        let inbound = string_meta(&self.meta, "message_id");
        match (
            chat.is_empty(),
            inbound.as_deref(),
            self.session_id.as_str(),
        ) {
            (_, Some(mid), _) if !mid.is_empty() && chat.is_empty() => mid.to_string(),
            (_, Some(mid), _) if !mid.is_empty() => format!("{chat}:{mid}"),
            (true, _, sid) if !sid.is_empty() => sid.to_string(),
            (false, _, sid) if !sid.is_empty() => format!("{chat}:{sid}"),
            _ => chat,
        }
    }

    pub fn reply_url(&self) -> Option<String> {
        string_meta(&self.meta, "reply_url").or_else(|| string_meta(&self.meta, "session_webhook"))
    }

    /// Forum topic / Feishu thread / Slack thread. Empty when the platform
    /// has no thread dimension (ordinary group chat).
    pub fn thread_id(&self) -> Option<String> {
        ["thread_id", "root_id", "topic_id", "message_thread_id"]
            .into_iter()
            .find_map(|key| string_meta(&self.meta, key))
            .filter(|s| !s.is_empty())
    }

    pub fn stamp_thread(&mut self, id: impl Into<String>) {
        let id = id.into();
        let id = id.trim();
        if id.is_empty() || id == "null" {
            return;
        }
        self.meta.insert("thread_id".into(), serde_json::json!(id));
        self.meta
            .insert("message_thread_id".into(), serde_json::json!(id));
    }

    /// Stable conversation identity. Endpoint, thread/topic, and (in groups)
    /// sender keep two people in one room from sharing an agent transcript.
    /// Hermes default: group sessions are per-user.
    pub fn route_key(&self) -> String {
        let ch = string_meta(&self.meta, "endpoint_id").unwrap_or_else(|| {
            if self.channel.is_empty() {
                "webhook"
            } else {
                self.channel.as_str()
            }
            .to_string()
        });
        let thread = self.thread_id();
        if self.is_group() {
            let mut key = match thread {
                Some(thread) => format!("{ch}:g:{}:t:{thread}", self.chat_id()),
                None => format!("{ch}:g:{}", self.chat_id()),
            };
            let who = if self.sender_id.is_empty() {
                self.chat_id()
            } else {
                self.sender_id.clone()
            };
            if !who.is_empty() {
                key.push_str(":u:");
                key.push_str(&who);
            }
            key
        } else {
            let who = if self.sender_id.is_empty() {
                self.chat_id()
            } else {
                self.sender_id.clone()
            };
            format!("{ch}:dm:{who}")
        }
    }

    pub fn merge(items: Vec<Self>) -> Option<Self> {
        let mut iter = items.into_iter();
        let mut first = iter.next()?;
        for next in iter {
            first.content_parts.extend(next.parts());
            if first.sender_id.is_empty() {
                first.sender_id = next.sender_id;
            }
            if first.sender_name.is_empty() {
                first.sender_name = next.sender_name;
            }
            for (k, v) in next.meta {
                first.meta.insert(k, v);
            }
            if first.session_id.is_empty() {
                first.session_id = next.session_id;
            }
        }
        first.text.clear();
        first.prompt.clear();
        Some(first)
    }
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn bool_meta(meta: &Map<String, Value>, key: &str) -> bool {
    match meta.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn string_meta(meta: &Map<String, Value>, key: &str) -> Option<String> {
    match meta.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_alias_becomes_parts() {
        let p = NativePayload::text_only("telegram", "hello");
        assert_eq!(p.query_text(), "hello");
        assert!(p.media_parts().is_empty());
        assert!(!p.is_choice_click());
        let mut click = NativePayload::text_only("feishu", "2");
        click.mark_choice_click();
        assert!(click.is_choice_click());
    }

    #[test]
    fn qwenpaw_image_part() {
        let raw = serde_json::json!({
            "channel_id": "telegram",
            "sender_id": "9",
            "content_parts": [
                {"type": "text", "text": "what color"},
                {"type": "image", "image_url": "https://x/a.png"}
            ],
            "meta": {"is_group": false, "chat_id": "9"}
        });
        let p: NativePayload = serde_json::from_value(raw).unwrap();
        assert_eq!(p.channel, "telegram");
        assert_eq!(p.query_text(), "what color");
        assert_eq!(p.media_parts().len(), 1);
        assert_eq!(p.route_key(), "telegram:dm:9");
    }

    #[test]
    fn group_message_carries_speaker_prefix() {
        let mut env = NativePayload::text_only("feishu", "这个方案不行");
        env.sender_id = "ou_1".into();
        env.sender_name = "小明".into();
        env.meta.insert("is_group".into(), json!(true));
        env.meta.insert("chat_id".into(), json!("g1"));
        let msg = env.to_chat_message();
        assert_eq!(msg.text(), "[小明] 这个方案不行");

        env.sender_name.clear();
        let msg = env.to_chat_message();
        assert_eq!(msg.text(), "[ou_1] 这个方案不行");

        env.meta.insert("is_group".into(), json!(false));
        let msg = env.to_chat_message();
        assert_eq!(msg.text(), "这个方案不行", "DM has no speaker prefix");
    }

    #[test]
    fn merge_concat_parts() {
        let a = NativePayload::text_only("hook", "a");
        let mut b = NativePayload::text_only("hook", "b");
        b.content_parts.push(ContentPart::Image {
            image_url: "https://x/i.png".into(),
            url: String::new(),
            mime: String::new(),
        });
        let m = NativePayload::merge(vec![a, b]).unwrap();
        assert!(m.query_text().contains('a'));
        assert_eq!(m.media_parts().len(), 1);
    }

    #[test]
    fn progress_bubble_key_prefers_inbound_message_id() {
        let mut env = NativePayload::text_only("telegram", "hi");
        env.meta.insert("chat_id".into(), json!("9"));
        env.session_id = "sess".into();
        assert_eq!(env.progress_bubble_key(), "9:sess");
        env.meta.insert("message_id".into(), json!(42));
        assert_eq!(env.progress_bubble_key(), "9:42");
        let mut hook = NativePayload::text_only("webhook", "hi");
        hook.session_id = "overnight-im-hb-1".into();
        assert_eq!(hook.progress_bubble_key(), "overnight-im-hb-1");
    }

    #[test]
    fn follow_up_text_keeps_route_and_drops_media() {
        let mut src = NativePayload::text_only("webhook", "long task");
        src.session_id = "s1".into();
        src.sender_id = "u1".into();
        src.content_parts.push(ContentPart::Image {
            image_url: "https://x/i.png".into(),
            url: String::new(),
            mime: String::new(),
        });
        let f = src.follow_up_text("also add tests");
        assert_eq!(f.session_id, "s1");
        assert_eq!(f.sender_id, "u1");
        assert_eq!(f.channel, "webhook");
        assert_eq!(f.query_text(), "also add tests");
        assert!(f.media_parts().is_empty());
    }

    #[test]
    fn route_key_is_scoped_by_endpoint_and_group_thread() {
        let mut env = NativePayload::text_only("feishu", "hi");
        env.sender_id = "u1".into();
        env.meta.insert("endpoint_id".into(), json!("work-bot"));
        assert_eq!(env.route_key(), "work-bot:dm:u1");

        env.meta.insert("is_group".into(), json!(true));
        env.meta.insert("chat_id".into(), json!("group-7"));
        env.meta.insert("thread_id".into(), json!("thread-2"));
        assert_eq!(
            env.route_key(),
            "work-bot:g:group-7:t:thread-2:u:u1",
            "group sessions are per-user; threads must not collide"
        );
        env.meta.insert("endpoint_id".into(), json!("personal-bot"));
        assert_eq!(env.route_key(), "personal-bot:g:group-7:t:thread-2:u:u1");
        env.sender_id = "u2".into();
        assert_eq!(
            env.route_key(),
            "personal-bot:g:group-7:t:thread-2:u:u2",
            "two speakers in the same thread keep separate transcripts"
        );
    }
}
