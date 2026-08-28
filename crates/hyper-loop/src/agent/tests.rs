use std::collections::VecDeque;
use std::sync::Mutex;

use super::dispatch::{
    canon_ws_path, observed_from_messages, parallel_safe_batch, search_fold_shrinks,
};
use super::notes::{forbids_tools, wants_auto_locate, wants_numeric_check, wants_web_check};
use super::turn::{
    NO_TOOL_THINK_FLOOR, PARSE_REPAIR_NOTE, PHYSICS_WRAP_NOTE, THINK_DIVERGENCE_NOTE,
};
use super::*;
use crate::error::Error;
use crate::permit;
use crate::policy::{Effort, ThinkPolicy};
use crate::session::{SessionEvent, SessionLog};
use crate::tools_schema::has_recall;
use serde_json::json;

struct Scripted {
    turns: Mutex<VecDeque<ModelTurn>>,
    meter: bool,
}

impl Completer for Scripted {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        self.turns
            .lock()
            .expect("script")
            .pop_front()
            .ok_or_else(|| Error::msg("script exhausted"))
    }

    fn prefix_meter(&self) -> Option<(Family, TemplateKwargs)> {
        if !self.meter {
            return None;
        }
        Some((
            Family::Qwen38,
            TemplateKwargs {
                enable_thinking: Some(false),
                reasoning_effort: None,
                preserve_thinking: None,
            },
        ))
    }
}

struct Delayed {
    inner: Scripted,
    delay: Duration,
}

impl Completer for Delayed {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        tokio::time::sleep(self.delay).await;
        self.inner.complete(messages, tools).await
    }
}

struct PolicyWatch {
    inner: Scripted,
    policy: Mutex<ThinkPolicy>,
    seen: std::sync::Arc<Mutex<Vec<ThinkPolicy>>>,
}

impl Completer for PolicyWatch {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        self.inner.complete(messages, tools).await
    }

    fn set_policy(&self, p: ThinkPolicy) {
        self.seen.lock().expect("seen").push(p.clone());
        *self.policy.lock().expect("policy") = p;
    }

    fn policy(&self) -> Option<ThinkPolicy> {
        Some(self.policy.lock().expect("policy").clone())
    }
}

fn turn_text(content: &str) -> ModelTurn {
    ModelTurn {
        content: content.into(),
        reasoning: String::new(),
        tool_calls: Vec::new(),
        raw_tool_calls: None,
        prompt_tokens: 1,
        completion_tokens: 1,
        watchdog_hit: false,
        parse_fail: false,
        cached_tokens: None,
        decode_tok_s: None,
        media: Vec::new(),
    }
}

fn turn_tool(name: &str, args: Value) -> ModelTurn {
    turn_tools(vec![("call_1", name, args)])
}

fn turn_said(content: &str, name: &str, args: Value) -> ModelTurn {
    let mut t = turn_tool(name, args);
    t.content = content.into();
    t
}

fn turn_tools(calls: Vec<(&str, &str, Value)>) -> ModelTurn {
    ModelTurn {
        content: String::new(),
        reasoning: String::new(),
        tool_calls: calls
            .into_iter()
            .map(|(id, name, arguments)| ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })
            .collect(),
        raw_tool_calls: None,
        prompt_tokens: 1,
        completion_tokens: 1,
        watchdog_hit: false,
        parse_fail: false,
        cached_tokens: None,
        decode_tok_s: None,
        media: Vec::new(),
    }
}

fn turn_parse_fail(content: &str) -> ModelTurn {
    ModelTurn {
        content: content.into(),
        reasoning: String::new(),
        tool_calls: Vec::new(),
        raw_tool_calls: None,
        prompt_tokens: 1,
        completion_tokens: 1,
        watchdog_hit: false,
        parse_fail: true,
        cached_tokens: None,
        decode_tok_s: None,
        media: Vec::new(),
    }
}

fn opts(dir: &std::path::Path) -> RunOpts {
    let mut o = RunOpts::from_config(&Config::default(), dir.to_path_buf());
    o.session_id = "test".into();
    o.print = false;
    o.max_steps = 8;
    o.agents_md = false;
    o.generation_reserve = 0;
    o.home = Some(dir.join(".hyper-home"));
    o.peripheral = true;
    o.skills_auto_catalog = false;
    o.mcp_auto_catalog = false;
    o.mcp = McpConfig::default();
    o.media_bins = MediaBins::none();
    // Tests assert minimal message geometry; narration is covered by its
    // own dedicated test below.
    o.narrate = false;
    o
}

#[test]
fn new_jsonl_stamps_channel() {
    let dir = std::env::temp_dir().join(format!("hyper-chan-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "im1".into();
    o.session_dir = Some(sess.clone());
    o.channel = "qq".into();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    };
    let _agent = Agent::new(scripted, o).unwrap();
    let log = SessionLog::open_in(&sess, "im1").unwrap();
    assert_eq!(log.start().unwrap().channel, "qq");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn text_only_stops() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([{
            let mut t = turn_text("done");
            t.reasoning = "brief".into();
            t
        }])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "done");
    assert_eq!(out.steps, 1);
    let asst = agent
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(asst.reasoning_content.as_deref(), Some("brief"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn plan_mode_blocks_write() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("nope.txt");
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("write", json!({"path": "nope.txt", "content": "secret"})),
            turn_text("## plan\n- leave the file alone"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.plan_mode = true;
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("plan a change").await.unwrap();
    assert!(out.text.contains("plan"));
    assert!(!target.exists(), "write must not land in plan mode");
    let tools: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.clone())
        .collect();
    assert!(
        tools.iter().any(|t| t.contains("plan mode")),
        "denied write: {tools:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn stop_before_complete_aborts_without_a_waiter() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let cancel = CancelFlag::new();
    cancel.cancel();
    agent.set_cancel(cancel);
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
    assert_eq!(out.steps, 0);
    assert!(out.text.is_empty(), "{}", out.text);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn stop_during_complete_does_not_wait_for_the_model() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let delayed = Delayed {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
            meter: false,
        },
        delay: Duration::from_secs(30),
    };
    let mut agent = Agent::new(delayed, opts(&dir)).unwrap();
    let cancel = CancelFlag::new();
    agent.set_cancel(cancel.clone());
    let h = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
    });
    let t0 = std::time::Instant::now();
    let out = agent.run("hi").await.unwrap();
    let _ = h.await;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "stop waited {:?}",
        t0.elapsed()
    );
    assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
    assert!(out.text.is_empty(), "{}", out.text);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn stop_during_bash_reaches_merged_cancel_flag() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("bash", json!({"command": "sleep 30"})),
            turn_text("should-not-run"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let cancel = CancelFlag::new();
    agent.set_cancel(cancel.clone());
    let h = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        cancel.cancel();
    });
    let t0 = std::time::Instant::now();
    let out = agent.run("sleep").await.unwrap();
    let _ = h.await;
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "bash ignore cancel, waited {:?}",
        t0.elapsed()
    );
    assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
    assert_ne!(out.text, "should-not-run");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn greeting_echo_line_is_stripped() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text(
            "你好\n\n有什么我可以帮你的吗？",
        )])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("你好").await.unwrap();
    assert_eq!(out.text, "你好\n\n有什么我可以帮你的吗？");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn tool_then_text() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "abc").unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "note.txt"})),
            turn_text("the file says abc"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("read the note").await.unwrap();
    assert!(out.text.contains("abc"));
    assert_eq!(out.steps, 2);
    let asst = agent
        .messages
        .iter()
        .find(|m| m.role == "assistant" && m.tool_calls.is_some())
        .expect("tool assistant");
    let args = &asst.tool_calls.as_ref().unwrap()[0]["function"]["arguments"];
    assert!(args.is_object(), "{args}");
    assert_eq!(args["path"], "note.txt");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn each_user_turn_resets_iteration_cap() {
    let dir = std::env::temp_dir().join(format!("hyper-iter-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "a.txt"})),
            turn_text("one"),
            turn_tool("read", json!({"path": "b.txt"})),
            turn_text("two"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.max_steps = 3;
    o.peripheral = false;
    let mut agent = Agent::new(scripted, o).unwrap();
    let first = agent.run("read a.txt").await.unwrap();
    assert_eq!(first.text, "one");
    assert!(
        first.stop_reason.as_deref().unwrap_or("").is_empty(),
        "first turn: {:?}",
        first.stop_reason
    );
    let second = agent.run("read b.txt").await.unwrap();
    assert_eq!(
        second.text, "two",
        "second turn hit {:?}",
        second.stop_reason
    );
    assert!(
        !second
            .stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Max iterations"),
        "iteration cap leaked across user turns: {:?}",
        second.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn physics_step_cap_wraps_then_keeps_spoken_text() {
    let dir = std::env::temp_dir().join(format!("hyper-wrap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 2;
    o.peripheral = false;
    let ping = json!({"path": "ping.txt"});
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_text("wrapped up"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt").await.unwrap();
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert_eq!(out.steps, 2);
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden.iter().all(|c| !c.contains(PHYSICS_WRAP_NOTE)),
        "Cursor path has no wrap lecture: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn steer_injects_after_tool_round() {
    let dir = std::env::temp_dir().join(format!("hyper-steer-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "abc").unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "note.txt"})),
            turn_text("ok focusing auth"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(vec!["focus on auth".into()]));
    agent.set_steer(slot);
    let out = agent.run("read the note").await.unwrap();
    assert!(out.text.contains("auth"));
    assert!(agent.messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().unwrap_or("").contains("focus on auth")
            && !crate::template::is_hidden_user_text(m.content.as_deref().unwrap_or(""))
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn edit_thrash_injects_guard_note_and_upgrades_effort() {
    let dir = std::env::temp_dir().join(format!("hyper-thrash-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "edit",
                json!({"path": "a.py", "old_string": "x = 1", "new_string": "x = 2"}),
            ),
            turn_tool(
                "edit",
                json!({"path": "a.py", "old_string": "x = 2", "new_string": "x = 1"}),
            ),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("fix a.py").await.unwrap();
    assert_eq!(out.text, "done");
    let guard_notes: Vec<&ChatMessage> = agent
        .messages
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] The same location was just reverted")
        })
        .collect();
    assert_eq!(guard_notes.len(), 0, "Cursor path has no thrash lecture");
    // The judgment upgrade must survive until the model's next turn; the
    // final clean text turn then drops it back to baseline.
    assert!(!agent.effort.auto_upgraded(), "clean step decays upgrade");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_expectation_edit_injects_guard_note_once() {
    let dir = std::env::temp_dir().join(format!("hyper-texp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/test_x.py"),
        "assertEqual(total, 1932.00)\nassertEqual(count, 3)\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "edit",
                json!({"path": "tests/test_x.py",
                       "old_string": "assertEqual(total, 1932.00)",
                       "new_string": "assertEqual(total, -1957.50)"}),
            ),
            turn_tool(
                "edit",
                json!({"path": "tests/test_x.py",
                       "old_string": "assertEqual(count, 3)",
                       "new_string": "assertEqual(count, 4)"}),
            ),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("修一下测试").await.unwrap();
    assert_eq!(out.text, "done");
    let notes = agent
        .messages
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] An existing test expectation was edited")
        })
        .count();
    assert_eq!(notes, 0, "Cursor path has no expectation lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_red_after_prod_edit_injects_guard_note() {
    let dir = std::env::temp_dir().join(format!("hyper-tred-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    std::fs::write(
        dir.join("test_app.py"),
        "import unittest\nfrom app import x\n\
         class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tools(vec![
                (
                    "e1",
                    "edit",
                    json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                ),
                (
                    "b1",
                    "bash",
                    json!({"command": format!("{} -B -m unittest test_app", crate::agent::verify::python_launcher())}),
                ),
            ]),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("fix app.py").await.unwrap();
    assert_eq!(out.text, "done");
    let notes = agent
        .messages
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] Tests went green to red after a production-only edit")
        })
        .count();
    assert_eq!(notes, 0, "Cursor path has no test-red lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn print_mode_test_red_is_advisory() {
    let dir = std::env::temp_dir().join(format!("hyper-tredp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    std::fs::write(
        dir.join("test_app.py"),
        "import unittest\nfrom app import x\n\
         class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tools(vec![
                (
                    "e1",
                    "edit",
                    json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                ),
                (
                    "b1",
                    "bash",
                    json!({"command": format!("{} -B -m unittest test_app", crate::agent::verify::python_launcher())}),
                ),
            ]),
            turn_text("should not run"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.print = true;
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("fix app.py").await.unwrap();
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert_eq!(out.text, "should not run");
    assert!(
        !agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] Tests went green to red after a production-only edit")
        }),
        "Cursor path has no test-red lecture"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn print_mode_oracle_reports_and_model_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-torc-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    std::fs::write(
        dir.join("test_app.py"),
        "import unittest\nfrom app import x\n\
         class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "edit",
                json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
            ),
            turn_text("should not run"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.print = true;
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("fix app.py").await.unwrap();
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert_eq!(out.text, "should not run");
    assert!(
        !agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] Tests went green to red after a production-only edit")
        }),
        "Cursor path has no test-red lecture"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn two_oracle_reds_bump_low_effort_once() {
    let dir = std::env::temp_dir().join(format!("hyper-teff-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    std::fs::write(
        dir.join("test_app.py"),
        "import unittest\nfrom app import x\n\
         class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
    )
    .unwrap();
    let low = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool(
                    "edit",
                    json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                ),
                turn_tool(
                    "edit",
                    json!({"path": "app.py", "old_string": "x = 2", "new_string": "x = 3"}),
                ),
                turn_text("done"),
            ])),
            meter: false,
        },
        policy: Mutex::new(low),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, opts(&dir)).unwrap();
    let out = agent.run("fix app.py").await.unwrap();
    assert_eq!(out.text, "done");
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter().all(|p| p.effort != Some(Effort::Medium)),
        "Cursor path does not bump effort from oracle reds: {seen:?}"
    );
    assert!(
        seen.iter().all(|p| p.effort != Some(Effort::Xhigh)),
        "must not auto-xhigh: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn write_over_existing_test_fires_expectation_note() {
    let dir = std::env::temp_dir().join(format!("hyper-twr-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("tests/test_x.py"), "assertEqual(n, 3)\n").unwrap();
    // Read first: the blind-overwrite gate refuses `write` to an existing
    // unobserved file before S1 can ever see it.
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "tests/test_x.py"})),
            turn_tool(
                "write",
                json!({"path": "tests/test_x.py", "content": "assertEqual(n, 4)\n"}),
            ),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("修一下测试").await.unwrap();
    assert_eq!(out.text, "done");
    let notes = agent
        .messages
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] An existing test expectation was edited")
        })
        .count();
    assert_eq!(notes, 0, "Cursor path has no expectation lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn coding_user_turn_injects_locate_spans() {
    let dir = std::env::temp_dir().join(format!("hyper-loc-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/paging.py"),
        "def page_bounds(n, size):\n    return (n - 1) * size, n * size\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("修 paging.py 的分页边界").await.unwrap();
    assert_eq!(out.text, "ok");
    let locate = agent.messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().unwrap_or("").contains("[locate]")
            && m.content.as_deref().unwrap_or("").contains("page_bounds")
    });
    assert!(!locate, "Cursor path must not inject Qwen locate cards");
    let _ = std::fs::remove_dir_all(dir);
}

struct Recasting(Scripted);
impl Completer for Recasting {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        self.0.complete(messages, tools).await
    }
    fn recasts_xai_product(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn grok_responses_path_skips_locate_card() {
    let dir =
        std::env::temp_dir().join(format!("hyper-loc-grok-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/paging.py"),
        "def page_bounds(n, size):\n    return (n - 1) * size, n * size\n",
    )
    .unwrap();
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("修 paging.py 的分页边界").await.unwrap();
    assert_eq!(out.text, "ok");
    let locate = agent
        .messages
        .iter()
        .any(|m| m.role == "user" && m.content.as_deref().unwrap_or("").contains("[locate]"));
    assert!(
        !locate,
        "Cursor/Responses path must not inject Qwen locate cards"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_skips_style_and_out_cards() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-cards-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    });
    let mut o = opts(&dir);
    o.narrate = true;
    o.peripheral = false;
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("现在几点").await.unwrap();
    assert_eq!(out.text, "ok");
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden
            .iter()
            .all(|c| !c.contains("[style]") && !c.contains("[out]")),
        "Cursor path must not inject style/out cards: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_dump_hop_finishes_without_lecture() {
    let dir =
        std::env::temp_dir().join(format!("hyper-grok-dump-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "write", json!({"path": "a.md", "content": essay})),
            turn_said(&again, "write", json!({"path": "b.md", "content": again})),
            turn_text("should-not-run"),
        ])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("how well does this fit the model").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert!(dir.join("a.md").is_file(), "first write should run");
    assert!(
        dir.join("b.md").is_file(),
        "Cursor executes the second write hop"
    );
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden
            .iter()
            .all(|c| !c.contains(crate::stutter::DUMP_NOTE)),
        "no dump lecture on Cursor path: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_identical_reads_halt_quietly() {
    let dir =
        std::env::temp_dir().join(format!("hyper-grok-doom-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    let ping = json!({"path": "ping.txt"});
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_text("should-not-run"),
        ])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt").await.unwrap();
    assert_eq!(out.stop_reason.as_deref(), None);
    assert_eq!(out.text, "should-not-run");
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden
            .iter()
            .all(|c| !c.contains(crate::paw_loop::REPEAT_NOTE)),
        "no repeat lecture on Cursor path: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_lossy_stutter_finishes_without_lecture() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-stutter-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("x\nx\nx\nx\n")])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "x\nx\nx\nx\n");
    assert_eq!(out.stop_reason, None);
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden
            .iter()
            .all(|c| !c.contains(crate::stutter::STUTTER_NOTE)),
        "Cursor path must not inject stutter lectures: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_quote_recap_of_read_hop_keeps_essay() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-quote-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let essay = "I studied the grok-hyper agent loop in detail.\n\
The core crate is hyper-loop and it runs a ReAct cycle.\n\
Frozen tools stay byte-stable across hops for cache hits.\n\
Template rendering uses the official chat template.\n\
Adapter builds OpenAI-compat requests for local Qwen.\n\
Sticky notes hold skill and MCP cards for the 27B.";
    let quoted = essay
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut o = opts(&dir);
    o.max_steps = 6;
    o.peripheral = false;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "read", json!({"path": "ping.txt"})),
            turn_text(&quoted),
        ])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert_eq!(out.text, quoted);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_empty_hop_then_file_quote_retries_then_keeps_real_answer() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-tool-recap-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let body = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    std::fs::write(dir.join("notes.md"), body).unwrap();
    let quoted = body
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let answer = "Next I will add a unit test in agent/mod.rs that writes ping.txt then \
reads it back. The test should use a Scripted completer and assert stop_reason is none. \
Then I will run cargo test -p hyper-loop --lib. After green, update the cron job prompt. \
This is a different task from architecture review and names different files on purpose.";
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "notes.md"})),
            turn_text(&quoted),
            turn_text(answer),
        ])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read notes.md").await.unwrap();
    assert_eq!(out.text, quoted);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_user_paste_quote_is_not_the_answer() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-user-echo-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let paste = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let quoted = paste
        .split(". ")
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let answer = "Next I will add a unit test in agent/mod.rs that writes ping.txt then \
reads it back. The test should use a Scripted completer and assert stop_reason is none. \
Then I will run cargo test -p hyper-loop --lib. After green, update the cron job prompt. \
This is a different task from architecture review and names different files on purpose.";
    let mut o = opts(&dir);
    o.max_steps = 6;
    o.peripheral = false;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([turn_text(&quoted), turn_text(answer)])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run(paste).await.unwrap();
    assert_eq!(out.text, quoted);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn grok_steer_is_plain_user_not_hidden_wrap() {
    let dir = std::env::temp_dir().join(format!(
        "hyper-grok-steer-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "abc").unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Recasting(Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "note.txt"})),
            turn_text("ok focusing auth"),
        ])),
        meter: false,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(vec!["focus on auth".into()]));
    agent.set_steer(slot);
    let out = agent.run("read the note").await.unwrap();
    assert!(out.text.contains("auth"));
    let steer = agent
        .messages
        .iter()
        .find(|m| m.role == "user" && m.content.as_deref().unwrap_or("").contains("focus on auth"))
        .expect("steer user turn");
    let body = steer.content.as_deref().unwrap_or("");
    assert!(
        !crate::template::is_hidden_user_text(body),
        "grok steer must not be a Qwen tool_response wrap: {body}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn bash_rg_dump_is_folded_to_spans() {
    let dir = std::env::temp_dir().join(format!("hyper-rg-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for f in 0..30 {
        let mut body = format!("def page_bounds_{f}(n):\n    return n\n");
        for i in 0..6 {
            body.push_str(&format!("# page_bounds note {f}-{i}\n"));
        }
        std::fs::write(dir.join(format!("src/f{f}.py")), &body).unwrap();
    }
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("bash", json!({"command": "grep -rn page_bounds src"})),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("修 page_bounds").await.unwrap();
    assert_eq!(out.text, "ok");
    let tool_msg = agent
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool message");
    let tool_txt = tool_msg.content.as_deref().unwrap_or("");
    assert!(
        tool_txt.contains("page_bounds") || tool_txt.contains("src/"),
        "{tool_txt}"
    );
    // grep -rn would emit 30 files x 8 matching lines.
    assert!(
        tool_txt.lines().count() < 240,
        "not shrunk: {} lines",
        tool_txt.lines().count()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn search_fold_only_when_it_shrinks() {
    let dump: String = (0..80).map(|i| format!("src/f{i}.py:1:hit\n")).collect();
    let spans = "## src/f0.py:1-4\n     1|def f():\n";
    assert!(search_fold_shrinks(&dump, spans));
    // One file matched end to end: spans are the file, folding would grow it.
    let small = "src/a.py:1:hit\nsrc/a.py:2:hit\n";
    let fat: String = (0..60).map(|i| format!("     {i}|line {i}\n")).collect();
    assert!(!search_fold_shrinks(small, &fat));
}

#[test]
fn auto_locate_only_on_coding_asks() {
    assert!(wants_auto_locate("修 paging.py 的分页"));
    assert!(wants_auto_locate("fix upgrade_medium"));
    assert!(wants_auto_locate(
        "cents() 用银行家舍入，财务说必须改成小学四舍五入。立刻改。不要改测试。"
    ));
    assert!(!wants_auto_locate("read the note"));
    assert!(!wants_auto_locate("where is the think cap upgraded"));
}

#[tokio::test]
async fn search_returns_function_span() {
    let dir = std::env::temp_dir().join(format!("hyper-search-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/policy.rs"),
        "pub struct ThinkPolicy { pub max_think_tokens: u32 }\n\n\
         fn upgrade_medium(&mut self) {\n    self.max_think_tokens = 2048;\n}\n",
    )
    .unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("search", json!({"query": "upgrade_medium"})),
            turn_text("found"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    assert!(crate::tools_schema::has_tool(&agent.tools, "Grep"));
    let out = agent.run("where is think cap upgraded").await.unwrap();
    assert_eq!(out.text, "found");
    let tool_txt = agent
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    assert!(tool_txt.contains("upgrade_medium"), "{tool_txt}");
    assert!(tool_txt.contains("src/policy.rs"), "{tool_txt}");
    assert!(
        !tool_txt.contains("struct ThinkPolicy"),
        "search dumped the whole file: {tool_txt}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn no_tool_phrase_widens_think_cap_then_restores() {
    let dir = std::env::temp_dir().join(format!("hyper-s6-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("答案")])),
            meter: false,
        },
        policy: Mutex::new(ThinkPolicy::agent_default()),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, opts(&dir)).unwrap();
    let out = agent.run("不要调用工具。纽科姆悖论怎么选？").await.unwrap();
    assert_eq!(out.text, "答案");
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter()
            .any(|p| p.max_think_tokens == NO_TOOL_THINK_FLOOR),
        "never widened: {seen:?}"
    );
    let last = seen.last().expect("set_policy");
    assert_eq!(last.max_think_tokens, 512, "must restore session policy");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn no_tool_policy_only_on_explicit_phrase() {
    assert!(forbids_tools("不要调用工具。什么是自由意志？"));
    assert!(forbids_tools("Please do not use tools. Explain Newcomb."));
    assert!(!forbids_tools("fix the parser in src/a.py"));
    assert!(!forbids_tools("if no tools are listed, skip"));
}

#[tokio::test]
async fn narrate_injects_style_card_once() {
    let dir = std::env::temp_dir().join(format!("hyper-style-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("好的"), turn_text("完成")])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.narrate = true;
    let mut agent = Agent::new(scripted, o).unwrap();
    agent.run("你好").await.unwrap();
    agent.run("再来").await.unwrap();
    let style_notes = agent
        .messages
        .iter()
        .filter(|m| m.role == "user" && m.content.as_deref().unwrap_or("").contains("[style]"))
        .count();
    assert_eq!(style_notes, 0, "Cursor path has no style card");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn print_mode_never_narrates() {
    let dir = std::env::temp_dir().join(format!("hyper-nonarr-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.narrate = true;
    o.print = true;
    let mut agent = Agent::new(scripted, o).unwrap();
    agent.run("hi").await.unwrap();
    assert!(agent
        .messages
        .iter()
        .all(|m| !m.content.as_deref().unwrap_or("").contains("[style]")));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn parallel_reads() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bravo\n").unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tools(vec![
                ("r1", "read", json!({"path": "a.txt"})),
                ("r2", "read", json!({"path": "b.txt"})),
            ]),
            turn_text("alpha bravo"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("read both").await.unwrap();
    assert_eq!(out.text, "alpha bravo");
    let tools: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| {
            (
                m.tool_call_id.clone().unwrap_or_default(),
                m.content.clone().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert_eq!(tools[0].0, "r1");
    assert_eq!(tools[1].0, "r2");
    assert!(tools[0].1.contains("alpha"), "{}", tools[0].1);
    assert!(tools[1].1.contains("bravo"), "{}", tools[1].1);
    let asst = agent
        .messages
        .iter()
        .find(|m| m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| c.len() == 2))
        .expect("parallel assistant");
    assert_eq!(asst.tool_calls.as_ref().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn parallel_safe_batch_only_read_and_view() {
    let call = |id: &str, name: &str| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    };
    let read = call("r", "read");
    let view = call("v", "view");
    assert!(parallel_safe_batch(&[read.clone(), view.clone()]));
    assert!(parallel_safe_batch(&[read.clone(), call("s", "search")]));
    assert!(parallel_safe_batch(&[read.clone(), call("g", "Grep")]));
    assert!(parallel_safe_batch(&[read.clone(), call("w", "WebSearch")]));
    assert!(parallel_safe_batch(&[call("r", "Read"), call("g", "Glob")]));
    assert!(parallel_safe_batch(&[
        call("t1", "Task"),
        call("t2", "Task")
    ]));
    assert!(parallel_safe_batch(&[read.clone(), call("t", "Task")]));
    assert!(!parallel_safe_batch(&[read.clone(), call("q", "ask")]));
    assert!(!parallel_safe_batch(&[
        read.clone(),
        call("q", "AskQuestion")
    ]));
    assert!(!parallel_safe_batch(&[read.clone()]));
    assert!(!parallel_safe_batch(&[read.clone(), call("m", "mcp")]));
    assert!(!parallel_safe_batch(&[
        call("q", "ask"),
        call("w", "write")
    ]));
    assert!(!parallel_safe_batch(&[
        call("r", "Read"),
        call("w", "Write")
    ]));
    assert!(!parallel_safe_batch(&[view, call("s", "skill")]));
    assert!(!parallel_safe_batch(&[
        call("a", "read"),
        call("b", "memory_search")
    ]));
}

#[tokio::test]
async fn prefix_budget_stops() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.working_window = 10;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
        meter: true,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert_eq!(out.text, "should not run");
    assert!(out.steps >= 1);
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden.iter().all(|c| !c.contains(PHYSICS_WRAP_NOTE)),
        "Cursor path has no wrap lecture: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compact_ratio_soft_limit_clamped() {
    assert_eq!(compact_soft_limit(1000, 0.70), 700);
    assert_eq!(compact_soft_limit(1000, 0.05), 100);
    assert_eq!(compact_soft_limit(1000, 2.0), 1000);
    assert_eq!(compact_soft_limit(262_144, 0.70), (262_144.0 * 0.70) as u32);
    assert_eq!(clamp_compact_ratio(f64::NAN), DEFAULT_COMPACT_RATIO);
}

#[test]
fn compact_soft_does_not_hard_fail() {
    assert!(over_soft_threshold(800, 0, 1000, 0.70));
    assert!(!over_hard_threshold(800, 0, 1000));
    assert!(over_hard_threshold(1001, 0, 1000));
    assert!(!over_soft_threshold(500, 0, 1000, 0.70));
    assert!(!over_soft_threshold(200_000, 0, 0, 0.70));
    assert!(!over_hard_threshold(200_000, 0, 0));
}

#[test]
fn turn_start_compact_at_120k_or_soft() {
    // 160k is under 262144 * 0.70 ≈ 183k but must compact on a follow-up.
    assert!(!over_soft_threshold(160_000, 0, 262_144, 0.70));
    assert!(should_compact_at_user_turn(160_000, 0, 262_144, 0.70));
    assert!(!should_compact_at_user_turn(100_000, 0, 262_144, 0.70));
    // Small-window tests hit the soft path, not a 120k fixture.
    assert!(should_compact_at_user_turn(800, 0, 1000, 0.70));
    assert!(!should_compact_at_user_turn(200_000, 0, 0, 0.70));
}

#[test]
fn follow_up_compacts_tool_heavy_even_under_120k() {
    assert!(!should_compact_at_user_turn(1_000, 0, 500_000, 0.80));
    assert!(should_compact_follow_up(1_000, 0, 500_000, 0.80, 8, 0));
    assert!(!should_compact_follow_up(1_000, 0, 500_000, 0.80, 7, 0));
    assert!(should_compact_follow_up(1_000, 0, 500_000, 0.80, 0, 5));
    assert!(!should_compact_follow_up(1_000, 0, 0, 0.80, 8, 5));
}

#[test]
fn mid_turn_compacts_computer_use_not_ordinary_reads() {
    assert!(!should_compact_mid_turn(8, 0));
    assert!(!should_compact_mid_turn(15, 4));
    assert!(should_compact_mid_turn(16, 0));
    assert!(should_compact_mid_turn(0, 5));
}

#[test]
fn window_262k_soft_and_hard_use_ratio_and_reserve() {
    let w = 262_144;
    let r = DEFAULT_GENERATION_RESERVE;
    let ratio = 0.80;
    // 262144 * 0.80 = 209715.2; prefix + 32768 > that ⇒ prefix ≥ 176948
    assert!(!over_soft_threshold(176_947, r, w, ratio));
    assert!(over_soft_threshold(176_948, r, w, ratio));
    assert!(!over_hard_threshold(w - r, r, w));
    assert!(over_hard_threshold(w - r + 1, r, w));
    assert_eq!(compact_soft_limit(w, ratio), 209_715);
}

#[tokio::test]
async fn prefix_hard_window_still_budgets() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.working_window = 10;
    o.compact_ratio = 0.10;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
        meter: true,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert_eq!(out.text, "should not run");
    assert!(out.steps >= 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn follow_up_archives_tool_heavy_turn_without_tokenizer() {
    let dir = std::env::temp_dir().join(format!("hyper-fu-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    for i in 0..8 {
        std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
    }
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "fu1".into();
    o.session_dir = Some(sess.clone());
    o.max_steps = 20;
    let mut turns = VecDeque::new();
    for i in 0..8 {
        turns.push_back(turn_tool_id(
            &format!("c{i}"),
            "read",
            json!({"path": format!("f{i}.txt")}),
        ));
    }
    turns.push_back(turn_text("first done"));
    turns.push_back(turn_text("second done"));
    let scripted = Scripted {
        turns: Mutex::new(turns),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let first = agent.run("read the eight files").await.unwrap();
    assert_eq!(first.text, "first done", "{:?}", first.stop_reason);
    let live_tools = agent.messages.iter().filter(|m| m.role == "tool").count();
    assert!(
        live_tools >= 8,
        "first turn should still hold its tools: {live_tools}"
    );
    let second = agent.run("what did you find").await.unwrap();
    assert_eq!(second.text, "second done", "{:?}", second.stop_reason);
    let live_tools = agent.messages.iter().filter(|m| m.role == "tool").count();
    assert_eq!(
        live_tools, 0,
        "follow-up must archive the previous tool turn, not replay it: {live_tools}"
    );
    let log = SessionLog::open_in(&sess, "fu1").unwrap();
    assert!(
        log.events()
            .iter()
            .any(|e| matches!(e, SessionEvent::Compact(_))),
        "tool-heavy follow-up should compact without a prefix meter: {:?}",
        log.events()
            .iter()
            .map(|e| e.type_name())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn prefix_soft_ratio_compacts_without_hard_error() {
    let dir = std::env::temp_dir().join(format!("hyper-soft-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "soft1".into();
    o.session_dir = Some(sess.clone());
    o.working_window = 8000;
    o.compact_ratio = 0.10;
    o.generation_reserve = 0;
    o.max_steps = 8;
    let blob = "W".repeat(8000);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool_id("c1", "write", json!({"path": "fat.txt", "content": blob})),
            turn_tool_id("c2", "read", json!({"path": "fat.txt"})),
            turn_text("first done"),
            turn_text("second done"),
        ])),
        meter: true,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let pad = "token ".repeat(400);
    let first = agent
        .run(&format!("task one then keep going {pad}"))
        .await
        .unwrap();
    assert!(
        !first
            .stop_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("budget:context"),
        "soft threshold must not budget:context: {:?}",
        first.stop_reason
    );
    let second = agent.run("follow up please").await.unwrap();
    assert_eq!(
        second.text, "second done",
        "follow-up hit {:?}",
        second.stop_reason
    );
    assert!(
        !second
            .stop_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("budget:context"),
        "soft compact must not fail the hard window: {:?}",
        second.stop_reason
    );
    let log = SessionLog::open_in(&sess, "soft1").unwrap();
    assert!(
        log.events()
            .iter()
            .any(|e| matches!(e, SessionEvent::Compact(_))),
        "follow-up over soft should archive previous turns: {:?}",
        log.events()
            .iter()
            .map(|e| e.type_name())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn malformed_then_valid_still_works() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_parse_fail("<tool_call>nope</tool_call>"),
            turn_text("fixed it"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "fixed it");
    assert_eq!(out.steps, 2);
    assert!(
        !agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("malformed") || c.contains("Resend a valid"))
        }),
        "parse retry must not lecture the model"
    );
    assert_eq!(
        agent
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .count(),
        1,
        "malformed step is dropped, not kept as an assistant turn"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn two_parse_fails_upgrade_then_clean_drops_back() {
    let dir = std::env::temp_dir().join(format!("hyper-effort-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_parse_fail("<tool_call>nope</tool_call>"),
                turn_parse_fail("<tool_call>still-bad</tool_call>"),
                turn_text("ok"),
            ])),
            meter: false,
        },
        policy: Mutex::new(ThinkPolicy::agent_default()),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "ok");
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter()
            .any(|p| p.effort == Some(Effort::Medium) && p.max_think_tokens == 2048),
        "never upgraded: {seen:?}"
    );
    let last = seen.last().expect("set_policy");
    assert_eq!(last.effort, Some(Effort::Low));
    assert_eq!(last.max_think_tokens, 512);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn watchdog_soft_nudge_recovers_without_policy_control() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            ModelTurn::watchdog(),
            turn_text("recovered"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "recovered");
    assert_eq!(out.steps, 2);
    assert!(agent.messages.iter().all(|m| {
        m.role != "user"
            || !m
                .content
                .as_deref()
                .unwrap_or("")
                .contains(THINK_DIVERGENCE_NOTE)
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn watchdog_second_cap_stops_without_disabling_thinking() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
    assert_eq!(start.max_tokens, 0);
    // 两连 watchdog：已经提醒并给过空间，按硬资源上限停止，不再用
    // thinking-off 答案替换模型自己的推理策略。
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                ModelTurn::watchdog(),
                ModelTurn::watchdog(),
                turn_text("should-not-run"),
            ])),
            meter: false,
        },
        policy: Mutex::new(start),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert!(out.text.is_empty(), "{}", out.text);
    assert_eq!(out.stop_reason, None);
    let seen = seen.lock().expect("seen").clone();
    assert_eq!(
        seen.iter().filter(|p| !p.enabled).count(),
        0,
        "watchdog must not replace model policy with thinking-off: {seen:?}"
    );
    assert!(
        seen.iter().any(|p| {
            p.enabled && p.max_think_tokens == NO_TOOL_THINK_FLOOR && p.max_tokens == 0
        }),
        "roomy retry must raise think floor without inventing a generation cap: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn watchdog_widens_thinking_and_keeps_it_enabled() {
    // M002 类：无工具轮 watchdog 命中，先按 NO_TOOL_THINK_FLOOR 升档重试，
    // 成功则全程不关思考。
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
    assert!(start.enabled && start.max_think_tokens < NO_TOOL_THINK_FLOOR);
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                ModelTurn::watchdog(),
                turn_text("recovered"),
            ])),
            meter: false,
        },
        policy: Mutex::new(start),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, opts(&dir)).unwrap();
    let out = agent.run("7^222 mod 1000 等于多少？").await.unwrap();
    assert_eq!(out.text, "recovered");
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter()
            .any(|p| p.enabled && p.max_think_tokens == NO_TOOL_THINK_FLOOR),
        "watchdog must retry with the widened think floor first: {seen:?}"
    );
    assert!(
        seen.iter().all(|p| p.enabled),
        "successful widened retry must keep the model-selected thinking mode: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn watchdog_after_tool_round_also_keeps_model_policy() {
    // 用过工具也不改变原则：事实提醒 + 原推理模式下的一次宽预算重试。
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt"})),
                ModelTurn::watchdog(),
                turn_text("recovered"),
            ])),
            meter: false,
        },
        policy: Mutex::new(start),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut agent = Agent::new(watch, o).unwrap();
    let out = agent.run("read ping.txt then answer").await.unwrap();
    assert_eq!(out.text, "recovered");
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter().all(|p| p.enabled),
        "tool-using retry must not disable the model's thinking: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|p| p.enabled && p.max_think_tokens == NO_TOOL_THINK_FLOOR),
        "tool-using turn should get the same roomy retry: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn watchdog_second_empty_cap_ends_quietly() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            ModelTurn::watchdog(),
            ModelTurn::watchdog(),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert!(out.text.is_empty(), "{}", out.text);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn doom_warns_once_then_lets_model_stop() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    let ping = json!({"path": "ping.txt"});
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_text("wrapped up"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt").await.unwrap();
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert_eq!(out.text, "wrapped up");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Doom loop"),
        "{:?}",
        out.stop_reason
    );
    let warns = hidden
        .iter()
        .filter(|c| c.contains(crate::paw_loop::REPEAT_NOTE))
        .count();
    assert_eq!(warns, 0, "Cursor path has no repeat lecture: {hidden:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn doom_warned_model_that_pivots_is_not_halted() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    std::fs::write(dir.join("pong.txt"), "ping\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool("read", json!({"path": "pong.txt"})),
            turn_text("pivoted"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read the files").await.unwrap();
    assert_eq!(out.text, "pivoted");
    assert!(
        !out.stop_reason.as_deref().unwrap_or("").contains("Doom"),
        "{:?}",
        out.stop_reason
    );
    let warned = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains(crate::paw_loop::REPEAT_NOTE));
    assert!(!warned, "Cursor path has no repeat lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn blind_overwrite_refused_until_read() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "precious original\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("write", json!({"path": "notes.md", "content": "blind"})),
            turn_tool("read", json!({"path": "notes.md"})),
            turn_tool("write", json!({"path": "notes.md", "content": "informed"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("rewrite notes.md").await.unwrap();
    assert_eq!(out.text, "done");
    let veto = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("already exists") && c.contains("notes.md"));
    assert!(!veto, "Cursor Write overwrites without a prior Read");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.md")).unwrap(),
        "informed",
        "post-read write must land"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn denied_writes_are_not_marked_observed_on_rebuild() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let ws = Workspace::open(&dir, true).unwrap();
    let write_call = |id: &str, path: &str| {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "write", "arguments": {"path": path, "content": "x"}}
        })
    };
    let mut msgs = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("task"),
        // 新契约文案（Error: 前缀）与三种旧失败文案都不得算已观察。
        ChatMessage::assistant_tools(None, vec![write_call("c1", "a.md")]),
        ChatMessage::tool("c1", permit::plan_denied("write")),
        ChatMessage::assistant_tools(None, vec![write_call("c2", "b.md")]),
        ChatMessage::tool(
            "c2",
            "plan mode: `write` blocked. Stay read-only and put the plan in your reply.",
        ),
        ChatMessage::assistant_tools(None, vec![write_call("c3", "c.md")]),
        ChatMessage::tool("c3", "User denied `write`. Continue without that call."),
        ChatMessage::assistant_tools(None, vec![write_call("c4", "d.md")]),
        ChatMessage::tool("c4", "tool task aborted"),
        // 成功的 write 仍然算已观察。
        ChatMessage::assistant_tools(None, vec![write_call("c5", "ok.md")]),
        ChatMessage::tool("c5", "Wrote 1 lines to ok.md"),
    ];
    let observed = observed_from_messages(&msgs, &ws);
    for denied in ["a.md", "b.md", "c.md", "d.md"] {
        assert!(
            !observed.contains(&canon_ws_path(&ws, denied)),
            "denied `{denied}` must not be observed: {observed:?}"
        );
    }
    assert!(observed.contains(&canon_ws_path(&ws, "ok.md")));
    // coordinator 中断文案同样不算。
    msgs.push(ChatMessage::assistant_tools(
        None,
        vec![write_call("c6", "e.md")],
    ));
    msgs.push(ChatMessage::tool("c6", "cancelled"));
    let observed = observed_from_messages(&msgs, &ws);
    assert!(!observed.contains(&canon_ws_path(&ws, "e.md")));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn plan_denied_write_stays_guarded_after_plan_go() {
    // P0 回归：plan 模式拒绝一次 write 后 /plan go，重建的 observed_paths
    // 不得把该路径当已观察 —— 盲覆写守卫必须仍然拦截。
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "precious original\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.plan_mode = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("write", json!({"path": "notes.md", "content": "blind"})),
            turn_text("## plan\n- rewrite notes.md"),
            // /plan go 后的第二轮：仍未 read 就 write，必须再次被守卫拦下。
            turn_tool(
                "write",
                json!({"path": "notes.md", "content": "still blind"}),
            ),
            turn_text("blocked again"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let _ = agent.run("plan a rewrite of notes.md").await.unwrap();
    agent.plan_mode = false;
    let out = agent.run("go implement").await.unwrap();
    assert_eq!(out.text, "blocked again");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.md")).unwrap(),
        "still blind",
        "after /plan go, Write overwrites like Cursor"
    );
    let veto = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("already exists") && c.contains("notes.md"));
    assert!(!veto, "no Qwen blind-overwrite guard");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn dot_slash_read_then_plain_write_passes_guard() {
    // canon_ws_path 回归：read("./a.rs") 后 write("a.rs") 不得被误拒。
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn old() {}\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "./a.rs"})),
            turn_tool("write", json!({"path": "a.rs", "content": "fn new() {}\n"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("rewrite a.rs").await.unwrap();
    assert_eq!(out.text, "done");
    let veto = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("already exists"));
    assert!(!veto, "./a.rs read must cover a.rs write");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.rs")).unwrap(),
        "fn new() {}\n"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn write_to_new_file_needs_no_read() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("write", json!({"path": "fresh.md", "content": "hello"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("create fresh.md").await.unwrap();
    assert_eq!(out.text, "done");
    assert_eq!(
        std::fs::read_to_string(dir.join("fresh.md")).unwrap(),
        "hello"
    );
    let veto = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("already exists"));
    assert!(!veto, "fresh files must not pay the read tax");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn web_check_trigger_is_narrow() {
    assert!(wants_web_check("2026年最新的 Rust 版本是多少？"));
    assert!(wants_web_check("苹果 M5 芯片什么时候发布？"));
    assert!(wants_web_check("看下 https://example.com/post 说了什么"));
    assert!(wants_web_check("what is the latest release of tokio?"));
    assert!(!wants_web_check("fix the parser in src/a.py"));
    assert!(!wants_web_check("把这个函数的价格字段改成 f64"));
    assert!(!wants_web_check("最新版本是多少？不要联网"));
    assert!(!wants_web_check("订单号 2026110234 查一下状态"));
}

#[test]
fn numeric_check_trigger_is_quantitative_and_non_code() {
    assert!(wants_numeric_check(
        "预测者准确率为 99%。比较两种决策论，并讨论错误率是否消除争议。"
    ));
    assert!(wants_numeric_check(
        "Calculate the probability threshold and distinguish percent from percentage points."
    ));
    assert!(!wants_numeric_check("这个模型准确率 99%，挺不错。"));
    assert!(!wants_numeric_check(
        "修复 accuracy.py 中把 99% 写成 0.99 的函数和测试用例。"
    ));
    assert!(!wants_numeric_check("比较两个哲学家的自由意志观点。"));
}

#[tokio::test]
async fn numeric_check_hint_is_one_short_task_local_card() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("one box"), turn_text("hello")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    agent
        .run("预测者准确率为 99%。比较两种理论，并讨论概率错误。")
        .await
        .unwrap();
    agent.run("你好").await.unwrap();
    let hints = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("[verify:numeric]"))
        .count();
    assert_eq!(hints, 0, "Cursor path has no numeric lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn web_hint_lands_only_on_fresh_questions() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_text("答：见来源。"),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    assert!(
        crate::tools_schema::has_tool(&agent.tools, "web"),
        "default config arms web"
    );
    let _ = agent.run("2026年最新的 Rust 稳定版是多少？").await.unwrap();
    let hinted = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("[web]"));
    assert!(!hinted, "Cursor path has no web hint lecture");

    let _ = agent.run("refactor the loop in main.rs").await.unwrap();
    let hints = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("[web]"))
        .count();
    assert_eq!(hints, 0, "Cursor path has no web hint lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn doc_read_card_lands_on_office_files_not_on_code() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_text("outline first"),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let _ = agent
        .run("请只读打开 HLX10-002-NSCLC301-CSR-v3-TOC-fixed.docx 的大纲")
        .await
        .unwrap();
    let hinted = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("[doc-read]"));
    assert!(!hinted, "Cursor path has no doc-read lecture");

    let _ = agent.run("refactor the loop in main.rs").await.unwrap();
    let hints = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("[doc-read]"))
        .count();
    assert_eq!(hints, 0, "Cursor path has no doc-read lecture");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn out_card_lands_each_user_turn() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("3pm"), turn_text("ok")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let _ = agent.run("现在几点").await.unwrap();
    let live = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("[out]") && !c.contains("[out] applied"))
        .count();
    assert_eq!(live, 0, "Cursor path has no out card");

    let _ = agent.run("谢谢").await.unwrap();
    let texts: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("[out]"))
        .collect();
    assert!(
        texts.iter().all(|c| !c.contains("[out]")),
        "Cursor path has no out card: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn doc_read_card_lands_after_glob_lists_office() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("report.docx"), b"pk").unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("Glob", json!({"glob_pattern": "**/*"})),
            turn_text("listed"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let _ = agent.run("工作区有哪些文件").await.unwrap();
    let hinted = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .any(|c| c.contains("[doc-read]"));
    assert!(!hinted, "Cursor path has no doc-read lecture after glob");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn overlay_window_injects_one_hidden_note() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.working_window = 8000;
    o.working_window_overlay = Some(WorkingWindowOverlay {
        from_file: crate::config::CODING_CTX_TOKENS,
        from_env: 8000,
    });
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let _ = agent.run("hi").await.unwrap();
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden.is_empty() || hidden.iter().all(|c| !c.contains("HYPER_WORKING_WINDOW")),
        "Cursor path has no window overlay lecture: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn doom_text_reply_before_halt_is_not_halt() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    let ping = json!({"path": "ping.txt"});
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_text("obsidian-compact"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt then stop").await.unwrap();
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert!(
        hidden.iter().all(|c| !c.contains("Repetitive pattern")),
        "no doom lecture: {hidden:?}"
    );
    assert_eq!(out.text, "obsidian-compact");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Doom loop"),
        "text reply must not inherit the tool-loop halt: {:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restated_dump_is_deferred_then_model_decides() {
    let dir = std::env::temp_dir().join(format!("hyper-restate-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "write", json!({"path": "a.md", "content": essay})),
            turn_said(&again, "write", json!({"path": "b.md", "content": again})),
            turn_text("should-not-run"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("how well does this fit the model").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert_eq!(out.text, "should-not-run");
    assert!(dir.join("a.md").is_file(), "first write should run");
    assert!(
        dir.join("b.md").is_file(),
        "Cursor executes the restated write hop"
    );
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    assert_eq!(
        hidden
            .iter()
            .filter(|c| c.contains(crate::stutter::DUMP_NOTE))
            .count(),
        0,
        "Cursor path has no dump lecture: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restated_dump_notes_once_then_defers_again() {
    let dir =
        std::env::temp_dir().join(format!("hyper-restate2-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "write", json!({"path": "a.md", "content": essay})),
            turn_said(&again, "write", json!({"path": "b.md", "content": again})),
            turn_said(&again, "write", json!({"path": "c.md", "content": again})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("how well does this fit the model").await.unwrap();
    assert_eq!(out.text, "done");
    assert!(dir.join("a.md").is_file());
    assert!(dir.join("b.md").is_file());
    assert!(dir.join("c.md").is_file());
    let notes = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains(crate::stutter::DUMP_NOTE))
        .count();
    assert_eq!(notes, 0, "dump lecture once");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn spoken_then_cleanup_is_deferred_not_hard_stopped() {
    let dir = std::env::temp_dir().join(format!("hyper-keeprm-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(
                essay,
                "write",
                json!({"path": "report.md", "content": essay}),
            ),
            turn_tool("bash", json!({"command": "rm -f report.md; echo done"})),
            turn_text("should-not-run"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("write a report").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert_eq!(out.text, "should-not-run");
    assert!(
        !dir.join("report.md").is_file(),
        "Cursor executes cleanup; it does not park the rm behind a dump lecture"
    );
    assert!(agent.messages.iter().all(|m| {
        m.role != "user"
            || m.content
                .as_deref()
                .map(|c| !c.contains(crate::stutter::DUMP_NOTE))
                .unwrap_or(true)
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restated_plus_unique_doc_write_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-docs-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let doc =
        "CONTRIBUTING\n\nFork the repo, open a PR against main, run cargo test -p hyper-loop \
--lib before you push. Do not bump the frozen tools array. Add a live scene only when the \
public llama.cpp endpoint is up. Name the branch after the ticket. Ask for review from William.";
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(
                essay,
                "write",
                json!({"path": "notes.md", "content": essay}),
            ),
            turn_said(
                &again,
                "write",
                json!({"path": "CONTRIBUTING.md", "content": doc}),
            ),
            turn_text("docs-done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent
        .run("write notes and a contributing guide")
        .await
        .unwrap();
    assert_eq!(out.text, "docs-done");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    assert!(dir.join("notes.md").is_file());
    assert!(dir.join("CONTRIBUTING.md").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restated_plus_edit_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-edit-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() { println!(\"a\"); }\n").unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "read", json!({"path": "main.rs"})),
            turn_said(
                &again,
                "edit",
                json!({
                    "path": "main.rs",
                    "old_string": "println!(\"a\")",
                    "new_string": "println!(\"b\")"
                }),
            ),
            turn_text("code-done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("fix the banner").await.unwrap();
    assert_eq!(out.text, "code-done");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    let body = std::fs::read_to_string(dir.join("main.rs")).unwrap();
    assert!(body.contains("println!(\"b\")"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn placeholder_write_then_cleanup_gets_soft_reassessment() {
    let dir =
        std::env::temp_dir().join(format!("hyper-ellipsis-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("loop.rs"), "fn drive() {}\n").unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let junk = dir.join("...").display().to_string();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "loop.rs"})),
            {
                let mut t = turn_tools(vec![
                    ("c1", "write", json!({"path": "...", "content": "..."})),
                    ("c2", "read", json!({"path": "loop.rs", "offset": 1})),
                ]);
                t.content = essay.into();
                t
            },
            {
                let mut t = turn_tools(vec![
                    (
                        "c3",
                        "bash",
                        json!({"command": format!("ls -la {junk} 2>/dev/null && cat {junk} && rm {junk} && echo REMOVED")}),
                    ),
                    ("c4", "read", json!({"path": "loop.rs"})),
                ]);
                t.content = "先清理一个误操作产生的杂散文件，然后继续读核心 loop。".into();
                t
            },
            turn_text("should-not-run"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("look at the core loop").await.unwrap();
    assert_eq!(out.stop_reason, None);
    assert_eq!(out.text, "should-not-run");
    assert!(
        !std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name() == "..."),
        "placeholder write must not land"
    );
    assert!(agent.messages.iter().all(|m| {
        m.role != "user"
            || m.content
                .as_deref()
                .map(|c| !c.contains(crate::stutter::DUMP_NOTE))
                .unwrap_or(true)
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn after_answer_new_file_read_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-newread-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "read", json!({"path": "a.rs"})),
            turn_said("再看一个文件。", "read", json!({"path": "b.rs"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read both files").await.unwrap();
    assert_eq!(out.text, "done");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restated_plus_send_bash_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-send-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
    let again = essay.replace("in detail", "carefully");
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(essay, "write", json!({"path": "out.md", "content": essay})),
            turn_said(
                &again,
                "bash",
                json!({"command": "cp out.md sent.md && echo sent"}),
            ),
            turn_text("sent-done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("write and copy the report").await.unwrap();
    assert_eq!(out.text, "sent-done");
    assert!(dir.join("sent.md").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn truncated_intro_promotes_write_body_into_reply() {
    let dir = std::env::temp_dir().join(format!("hyper-promote-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let stub = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template.";
    let body = format!(
        "{stub} Adapter builds OpenAI-compat requests. Sticky notes hold skill and MCP cards. \
This is a strong fit for the 27B local model because the prefix is byte-stable."
    );
    let mut o = opts(&dir);
    o.max_steps = 4;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(stub, "write", json!({"path": "report.md", "content": body})),
            turn_text("stop-here"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("analyze the harness").await.unwrap();
    assert_eq!(out.text, "stop-here");
    let spoken = std::fs::read_to_string(dir.join("report.md")).unwrap_or_default();
    assert!(
        spoken.contains("byte-stable"),
        "write body lands in the file: {spoken:?}"
    );
    assert!(dir.join("report.md").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn short_status_plus_tools_is_not_a_restate() {
    let dir = std::env::temp_dir().join(format!("hyper-status-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let ping = json!({"path": "ping.txt"});
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said("好的，我继续读。", "read", ping.clone()),
            turn_said(
                "好的，我继续读。",
                "read",
                json!({"path": "ping.txt", "offset": 1}),
            ),
            turn_text("obsidian-compact"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt then stop").await.unwrap();
    assert_eq!(out.text, "obsidian-compact");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn long_narration_plus_reread_is_not_a_dump() {
    let dir = std::env::temp_dir().join(format!("hyper-narrate-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("loop.rs"), "fn drive() {}\n").unwrap();
    let talk = "`parallel_safe_batch` restricts parallel to read and view only, so the \
dispatch asymmetry is safe in practice. Let me check run, tool surface building, \
and the system prompt assembly next.";
    assert!(
        crate::stutter::is_substantial_reply(talk),
        "fixture must be long enough to have tripped the old lock"
    );
    let mut o = opts(&dir);
    o.max_steps = 8;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(talk, "read", json!({"path": "loop.rs"})),
            turn_said(talk, "read", json!({"path": "loop.rs", "offset": 1})),
            turn_tool("bash", json!({"command": "grep -n drive loop.rs"})),
            turn_text("wiring looks sound. unique reads and grep are real work."),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("review the wiring").await.unwrap();
    assert!(out.text.contains("wiring looks sound"), "{}", out.text);
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn checkpoint_then_expanded_answer_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-expand-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let essay = "I studied the grok-hyper agent loop in detail. The core crate is hyper-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen. Wiring of skills and mcp is a hidden-card overlay.";
    let checkpoint: String = essay.chars().take(220).collect();
    assert!(crate::stutter::is_substantial_reply(&checkpoint));
    let mut o = opts(&dir);
    o.max_steps = 6;
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_said(
                &checkpoint,
                "write",
                json!({"path": "notes.md", "content": checkpoint}),
            ),
            turn_text(essay),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("review the loop").await.unwrap();
    assert_eq!(out.text, essay);
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("budget:repeat"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_doom_notes_once_then_lets_model_stop() {
    let dir = std::env::temp_dir().join(format!("hyper-lossy-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let ping = json!({"path": "ping.txt"});
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", ping.clone()),
            turn_tool("read", ping.clone()),
            turn_text("wrapped up"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping.txt").await.unwrap();
    assert_eq!(out.text, "wrapped up");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Doom loop"),
        "{:?}",
        out.stop_reason
    );
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    let warns = hidden
        .iter()
        .filter(|c| c.contains(crate::paw_loop::REPEAT_NOTE))
        .count();
    assert_eq!(warns, 0, "repeat fact lands exactly once: {hidden:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_name_streak_allows_distinct_bash_commands() {
    let dir = std::env::temp_dir().join(format!("hyper-streak2-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("bash", json!({"command": "true"})),
            turn_tool("bash", json!({"command": "echo a"})),
            turn_tool("bash", json!({"command": "echo b"})),
            turn_tool("bash", json!({"command": "echo c"})),
            turn_text("explored"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("run commands").await.unwrap();
    assert_eq!(out.text, "explored");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Name streak"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_read_edit_read_is_not_name_streak() {
    let dir = std::env::temp_dir().join(format!("hyper-rer-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool(
                "edit",
                json!({"path": "ping.txt", "old_string": "pong", "new_string": "pong"}),
            ),
            turn_tool("read", json!({"path": "other.txt"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("tweak ping").await.unwrap();
    assert_eq!(out.text, "done");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Name streak"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_read_edit_read_same_path_is_not_path_loop() {
    let dir = std::env::temp_dir().join(format!("hyper-rer2-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool(
                "edit",
                json!({"path": "ping.txt", "old_string": "pong", "new_string": "ping"}),
            ),
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("tweak ping").await.unwrap();
    assert_eq!(out.text, "done");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Path loop"),
        "{:?}",
        out.stop_reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_same_path_edits_note_once_then_continue() {
    // 分页 read 不算 Path loop；一字不差的重读由 doom 观察。
    // 同路径连续 edit/write 只注入一次轨迹观察，然后交给模型。
    let dir = std::env::temp_dir().join(format!("hyper-path-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "ping.txt"})),
            turn_tool(
                "edit",
                json!({"path": "ping.txt", "old_string": "pong", "new_string": "p1"}),
            ),
            turn_tool(
                "edit",
                json!({"path": "ping.txt", "old_string": "p1", "new_string": "p2"}),
            ),
            turn_tool(
                "edit",
                json!({"path": "ping.txt", "old_string": "p2", "new_string": "p3"}),
            ),
            turn_text("wrapped up"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("edit ping").await.unwrap();
    assert_eq!(out.text, "wrapped up");
    assert!(
        !out.stop_reason
            .as_deref()
            .unwrap_or("")
            .contains("Path loop"),
        "{:?}",
        out.stop_reason
    );
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    let notes = hidden
        .iter()
        .filter(|c| c.contains(crate::paw_loop::PATH_NOTE))
        .count();
    assert_eq!(notes, 0, "path fact lands exactly once: {hidden:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_paged_reads_same_path_do_not_halt() {
    // 压缩注记教模型用 offset 翻页，翻页不得被 PathLoopGate 斩断。
    let dir = std::env::temp_dir().join(format!("hyper-page-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ping.txt"), "pong\n".repeat(50)).unwrap();
    let mut o = opts(&dir);
    o.max_steps = 12;
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "ping.txt", "offset": 0})),
            turn_tool("read", json!({"path": "ping.txt", "offset": 10})),
            turn_tool("read", json!({"path": "ping.txt", "offset": 20})),
            turn_text("paged"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("read ping").await.unwrap();
    assert_eq!(out.text, "paged", "{:?}", out.stop_reason);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_stutter_notes_once_then_model_continues() {
    let dir = std::env::temp_dir().join(format!("hyper-stutter-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("x\nx\nx\nx\n"), turn_text("ok")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "x\nx\nx\nx\n");
    assert_eq!(out.stop_reason, None);
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .filter(|c| crate::template::is_hidden_user_text(c))
        .collect();
    let notes = hidden
        .iter()
        .filter(|c| c.contains(crate::stutter::STUTTER_NOTE))
        .count();
    assert_eq!(notes, 0, "Cursor path has no stutter lecture: {hidden:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_two_parse_fails_stop() {
    let dir = std::env::temp_dir().join(format!("hyper-parse-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    o.low_precision = true;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_parse_fail("<tool_call>nope</tool_call>"),
            turn_parse_fail("<tool_call>still-bad</tool_call>"),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "ok");
    assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
    assert!(agent.messages.iter().all(|m| {
        !m.content
            .as_deref()
            .unwrap_or("")
            .contains(PARSE_REPAIR_NOTE)
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn lossy_think_cap_on_default_low() {
    let dir = std::env::temp_dir().join(format!("hyper-cap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let watch = PolicyWatch {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok")])),
            meter: false,
        },
        policy: Mutex::new(ThinkPolicy::agent_default()),
        seen: std::sync::Arc::new(Mutex::new(Vec::new())),
    };
    let seen = watch.seen.clone();
    let mut o = opts(&dir);
    o.low_precision = true;
    let mut agent = Agent::new(watch, o).unwrap();
    let _ = agent.run("hi").await.unwrap();
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter()
            .all(|p| p.max_think_tokens != crate::policy::LOSSY_THINK_CAP),
        "Cursor path does not apply the 27B lossy think cap: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn turn_tool_id(id: &str, name: &str, args: Value) -> ModelTurn {
    let mut t = turn_tool(name, args);
    t.tool_calls[0].id = id.into();
    t
}

#[tokio::test]
async fn prefix_budget_compacts_then_runs() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "c2".into();
    o.session_dir = Some(sess.clone());
    o.working_window = 2800;
    o.generation_reserve = 0;
    o.max_steps = 12;
    let home = o.home.clone();
    let blob = "W".repeat(8000);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool_id("c1", "write", json!({"path": "fat.txt", "content": blob})),
            turn_tool_id("c2", "read", json!({"path": "fat.txt"})),
            turn_tool_id(
                "c3",
                "bash",
                json!({"command": "python3 -c \"print('Y'*8000)\""}),
            ),
            turn_text("done after compact"),
        ])),
        meter: true,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("task one then keep going").await.unwrap();
    let reason = out.stop_reason.clone().unwrap_or_default();
    assert!(
        out.text.contains("done after compact") || reason.starts_with("budget:context"),
        "text={} reason={reason}",
        out.text
    );
    let log = SessionLog::open_in(&sess, "c2").unwrap();
    let kinds: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
    if kinds.iter().any(|k| *k == "session/compact") {
        assert!(has_recall(&agent.tools), "recall appended after compact");
        let live_users: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert!(
            live_users
                .iter()
                .any(|c| crate::template::is_hidden_user_text(c)),
            "archive must be hidden: {live_users:?}"
        );
        assert!(
            log.events()
                .iter()
                .any(|e| matches!(e, SessionEvent::User(u) if u.text.contains("task one"))),
            "JSONL still has the original user"
        );
        let notes = std::fs::read_dir(home.as_ref().unwrap().join("memory"))
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        assert!(notes > 0, "compact should write a daily note under memory/");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn persist_session_writes_jsonl() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "p1".into();
    o.session_dir = Some(sess.clone());
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("done")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "done");
    assert_eq!(out.session_id, "p1");
    let log = SessionLog::open_in(&sess, "p1").unwrap();
    let kinds: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
    assert_eq!(kinds, ["session/start", "user", "assistant", "stop"]);
    assert_eq!(log.messages().len(), 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn emit_sink_streams_tool_then_assistant_not_user() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "abc\n").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "note.txt"})),
            turn_text("abc"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
    agent.set_emit(crate::sidecar::EventSink { tx });
    let out = agent.run("read the note").await.unwrap();
    assert_eq!(out.text, "abc");
    let mut kinds = Vec::new();
    while let Ok(e) = rx.try_recv() {
        kinds.push(e.type_name().to_string());
    }
    assert!(
        !kinds.iter().any(|k| k == "user"),
        "live user/skill cards stay off the TUI stream: {kinds:?}"
    );
    assert!(kinds.iter().any(|k| k == "tool"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "assistant"), "{kinds:?}");
    assert_eq!(kinds.last().map(String::as_str), Some("stop"), "{kinds:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn child_emit_stays_off_parent_stream() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "abc\n").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("read", json!({"path": "note.txt"})),
            turn_text("abc"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.child = Some(crate::subagent::ChildCtx {
        kind: crate::subagent::SubagentType::Explore,
        capability: crate::subagent::CapabilityMode::ReadOnly,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    agent.set_emit(crate::sidecar::EventSink { tx });
    let out = agent.run("read the note").await.unwrap();
    assert_eq!(out.text, "abc");
    let mut kinds = Vec::new();
    while let Ok(e) = rx.try_recv() {
        kinds.push(e.type_name().to_string());
    }
    assert!(
        kinds.is_empty(),
        "child live sink must not pollute the parent stream: {kinds:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn child_tools_omit_task() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::new()),
        meter: false,
    };
    let mut o = opts(&dir);
    o.child = Some(crate::subagent::ChildCtx {
        kind: crate::subagent::SubagentType::GeneralPurpose,
        capability: crate::subagent::CapabilityMode::All,
    });
    let agent = Agent::new(scripted, o).unwrap();
    assert!(
        crate::tools_schema::has_tool(agent.tools(), "Read"),
        "children keep Read"
    );
    assert!(
        crate::tools_schema::has_tool(agent.tools(), "AwaitShell"),
        "children keep AwaitShell"
    );
    assert!(
        !crate::tools_schema::has_tool(agent.tools(), "Task"),
        "children must not see Task"
    );
    assert!(
        !crate::tools_schema::has_tool(agent.tools(), "ComputerUse"),
        "children must not see ComputerUse"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_write_uses_parent_permit_hub() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (hub, mut rx) = crate::permit::PermitHub::pair(crate::permit::ApprovalMode::Ask);
    let waiter = tokio::spawn(async move {
        let req = rx.recv().await.expect("permit ask");
        let _ = req.reply.send(crate::permit::PermitDecision::Deny);
    });
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("Write", json!({"path": "out.txt", "contents": "nope"})),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.child = Some(crate::subagent::ChildCtx {
        kind: crate::subagent::SubagentType::GeneralPurpose,
        capability: crate::subagent::CapabilityMode::All,
    });
    o.permit = Some(hub);
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("write it").await.unwrap();
    waiter.await.unwrap();
    assert_eq!(out.text, "ok");
    let joined = agent
        .messages()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("User denied") || joined.contains("denied"),
        "{joined}"
    );
    assert!(!dir.join("out.txt").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_ask_uses_parent_clarify_hub() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (hub, mut rx) = crate::clarify::ClarifyHub::pair();
    let waiter = tokio::spawn(async move {
        let req = rx.recv().await.expect("clarify ask");
        let _ = req.reply.send(crate::clarify::ClarifyDecision::Skip);
    });
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "AskQuestion",
                json!({
                    "questions": [{
                        "prompt": "which?",
                        "options": [
                            {"id": "a", "label": "A"},
                            {"id": "b", "label": "B"}
                        ]
                    }]
                }),
            ),
            turn_text("picked"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.child = Some(crate::subagent::ChildCtx {
        kind: crate::subagent::SubagentType::Office,
        capability: crate::subagent::CapabilityMode::ReadWrite,
    });
    o.clarify = Some(hub);
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("ask").await.unwrap();
    waiter.await.unwrap();
    assert_eq!(out.text, "picked");
    let joined = agent
        .messages()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("interactive channel"), "{joined}");
    assert!(
        joined.contains("skipped") || joined.contains("recommended"),
        "{joined}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn child_ask_without_clarify_hub_errors() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "AskQuestion",
                json!({
                    "prompt": "which?",
                    "options": [
                        {"id": "a", "label": "A"},
                        {"id": "b", "label": "B"}
                    ]
                }),
            ),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.child = Some(crate::subagent::ChildCtx {
        kind: crate::subagent::SubagentType::Office,
        capability: crate::subagent::CapabilityMode::ReadWrite,
    });
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("ask").await.unwrap();
    assert_eq!(out.text, "done");
    let joined = agent
        .messages()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("interactive channel"), "{joined}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn im_ask_without_clarify_hub_skips() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool(
                "AskQuestion",
                json!({
                    "prompt": "which?",
                    "options": [
                        {"id": "a", "label": "A"},
                        {"id": "b", "label": "B"}
                    ]
                }),
            ),
            turn_text("done"),
        ])),
        meter: false,
    };
    let mut o = opts(&dir);
    o.channel = "wechat".into();
    let mut agent = Agent::new(scripted, o).unwrap();
    let out = agent.run("ask").await.unwrap();
    assert_eq!(out.text, "done");
    let joined = agent
        .messages()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("skipped: using A"), "{joined}");
    assert!(!joined.contains("interactive channel"), "{joined}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unattended_im_uses_hermes_caps() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    let mut o = opts(&dir);
    o.channel = "wechat".into();
    crate::agent::apply_unattended_policy(&mut o, &Config::default());
    assert_eq!(o.max_steps, 500);
    assert!(o.max_wall.is_zero());
    o.channel = "web".into();
    o.max_steps = 80;
    o.max_wall = std::time::Duration::from_secs(1800);
    crate::agent::apply_unattended_policy(&mut o, &Config::default());
    assert_eq!(o.max_steps, 80);
    assert_eq!(o.max_wall, std::time::Duration::from_secs(1800));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn token_deltas_reach_sink_before_assistant_and_skip_jsonl() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    let sess = dir.join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    struct StreamingScripted {
        turn: Mutex<Option<ModelTurn>>,
        sink: Mutex<Option<TokenSink>>,
    }
    impl Completer for StreamingScripted {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            let turn = self
                .turn
                .lock()
                .expect("turn")
                .take()
                .ok_or_else(|| Error::msg("script exhausted"))?;
            if let Some(sink) = self.sink.lock().expect("sink").clone() {
                sink.reset();
                for ch in turn.reasoning.chars() {
                    sink.reasoning(&ch.to_string());
                }
                for ch in turn.content.chars() {
                    sink.content(&ch.to_string());
                }
            }
            Ok(turn)
        }

        fn set_token_sink(&self, sink: Option<TokenSink>) {
            *self.sink.lock().expect("sink") = sink;
        }
    }

    let mut reasoned = turn_text("hello");
    reasoned.reasoning = "hmm".into();
    let mut o = opts(&dir);
    o.persist_session = true;
    o.session_id = "delta1".into();
    o.session_dir = Some(sess.clone());
    let mut agent = Agent::new(
        StreamingScripted {
            turn: Mutex::new(Some(reasoned)),
            sink: Mutex::new(None),
        },
        o,
    )
    .unwrap();
    agent.set_emit(crate::sidecar::EventSink { tx });
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "hello");
    let mut kinds = Vec::new();
    let mut reasoning = String::new();
    let mut content = String::new();
    let mut saw_reset = false;
    while let Ok(e) = rx.try_recv() {
        kinds.push(e.type_name().to_string());
        if let SessionEvent::Delta(d) = e {
            if d.reset {
                saw_reset = true;
            } else if d.channel == crate::session::DeltaChannel::Reasoning {
                reasoning.push_str(&d.text);
            } else {
                content.push_str(&d.text);
            }
        }
    }
    let delta_at = kinds.iter().position(|k| k == "delta").expect("delta");
    let assistant_at = kinds
        .iter()
        .position(|k| k == "assistant")
        .expect("assistant");
    assert!(delta_at < assistant_at, "{kinds:?}");
    assert!(saw_reset);
    assert_eq!(reasoning.replace(crate::llm_http::CONNECT_HINT, ""), "hmm");
    assert_eq!(content, "hello");
    let log = SessionLog::open_in(&sess, "delta1").unwrap();
    let persisted: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
    assert_eq!(persisted, ["session/start", "user", "assistant", "stop"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn peripheral_off_keeps_frozen_four() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut o = opts(&dir);
    o.peripheral = false;
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("done")])),
        meter: false,
    };
    let agent = Agent::new(scripted, o).unwrap();
    let names: Vec<_> = agent
        .tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(&names[..4], ["Read", "Write", "StrReplace", "Delete"]);
    assert!(names.contains(&"Grep"));
    assert!(names.contains(&"Shell"));
    assert!(names.contains(&"AskQuestion"));
    assert!(names.contains(&"Task"));
    assert!(!names.contains(&"read"));
    assert!(!names.contains(&"bash"));
    assert!(names.contains(&"view"));
    assert!(names.contains(&"ComputerUse"));
    assert!(!names.contains(&"memory_search"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn memory_search_and_skill_dispatch() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join(".hyper-home");
    let skill_dir = home.join("skills").join("pdf");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext on the file.\n",
    )
    .unwrap();
    let mut o = opts(&dir);
    o.home = Some(home.clone());
    let store = crate::memory::MemoryStore::open(&home).unwrap();
    store
        .write_compact_note("s", 1, "read crates/foo.rs linker rewrite")
        .unwrap();

    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("memory_search", json!({"query": "linker"})),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    agent.tools.push(crate::tools_schema::memory_search_tool());
    assert!(crate::tools_schema::has_tool(&agent.tools, "memory_search"));
    assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
    assert!(!crate::tools_schema::has_tool(&agent.tools, "mcp"));
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(!sys.contains("MEMORY.md"));
    assert!(!sys.contains("pdf"));
    assert!(
        !sys.contains("Use pdftotext on the file"),
        "SKILL.md body must not be in system"
    );
    let out = agent.run("search then pdf").await.unwrap();
    assert_eq!(out.text, "ok");
    let tools: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        tools
            .iter()
            .any(|t| t.contains("linker") || t.contains("foo.rs")),
        "{tools:?}"
    );
    let hidden: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        !hidden.iter().any(|t| t.contains("pdftotext")),
        "Cursor path injects skills only on an explicit [skill:] prefix, got {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn emitted_skill_call_runs_without_tools_entry() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join(".hyper-home");
    let skill_dir = home.join("skills").join("pdf");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext on the file.\n",
    )
    .unwrap();
    let mut o = opts(&dir);
    o.home = Some(home);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("skill", json!({"name": "pdf"})),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
    let out = agent.run("extract the pdf").await.unwrap();
    assert_eq!(out.text, "ok");
    let tools: Vec<_> = agent
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        tools
            .iter()
            .any(|t| t.contains("Use pdftotext on the file")),
        "skill call must return the body, got {tools:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn empty_script() -> Scripted {
    Scripted {
        turns: Mutex::new(VecDeque::new()),
        meter: false,
    }
}

fn hidden_texts(agent: &Agent<Scripted>) -> Vec<String> {
    agent
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

#[test]
fn agent_md_line_is_frozen_system() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("AGENT.md"), "客官来了。得嘞。不列清单。\n").unwrap();
    let agent = Agent::new(empty_script(), opts(&dir)).unwrap();
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(sys.contains("客官来了。得嘞。不列清单。"), "{sys}");
    assert!(!sys.contains("MEMORY.md"), "{sys}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agents_md_omitted_when_over_cap() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut body = String::from("HEAD_UNIQUE_AGENTS\n");
    for i in 0..200 {
        body.push_str(&format!(
            "workspace convention number {i} must be followed by all edits.\n"
        ));
    }
    body.push_str("TAIL_UNIQUE_AGENTS\n");
    std::fs::write(dir.join("AGENTS.md"), &body).unwrap();
    let mut o = opts(&dir);
    o.agents_md = true;
    o.agents_md_max_tokens = 80;
    o.agents_md_head = false;
    let agent = Agent::new(empty_script(), o).unwrap();
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(!sys.contains("HEAD_UNIQUE_AGENTS"), "{sys}");
    assert!(!sys.contains("TAIL_UNIQUE_AGENTS"), "{sys}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agents_md_head_clips_instead_of_omitting() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut body = String::from("HEAD_UNIQUE_AGENTS\n");
    for i in 0..200 {
        body.push_str(&format!(
            "workspace convention number {i} must be followed by all edits.\n"
        ));
    }
    body.push_str("TAIL_UNIQUE_AGENTS\n");
    std::fs::write(dir.join("AGENTS.md"), &body).unwrap();
    let mut o = opts(&dir);
    o.agents_md = true;
    o.agents_md_max_tokens = 80;
    o.agents_md_head = true;
    let agent = Agent::new(empty_script(), o).unwrap();
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(sys.contains("HEAD_UNIQUE_AGENTS"), "{sys}");
    assert!(!sys.contains("TAIL_UNIQUE_AGENTS"), "{sys}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn from_config_enables_agents_md() {
    let o = RunOpts::from_config(
        &crate::config::Config::default(),
        std::path::PathBuf::from("/tmp"),
    );
    assert!(o.agents_md);
    assert!(o.computer_use);
}

#[tokio::test]
async fn memory_hot_card_on_commit_not_on_yesno() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join(".hyper-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("MEMORY.md"),
        "# Prefs\n- 回复中文\n- commit: conv\n# Hosts\n- ssh = ops@192.0.2.8\n",
    )
    .unwrap();
    let mut o = opts(&dir);
    o.home = Some(home);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("是"), turn_text("fix: foo")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(!sys.contains("MEMORY.md"));
    assert!(!sys.contains("ops@192.0.2.8"));
    agent
        .run("这个函数 off-by-one 吗？只答是或否。")
        .await
        .unwrap();
    let after_yes = hidden_texts(&agent);
    assert!(
        !after_yes.iter().any(|t| t.contains("MEMORY")),
        "{after_yes:?}"
    );
    agent.run("写一条 commit 标题").await.unwrap();
    let after_commit = hidden_texts(&agent);
    assert!(
        !after_commit.iter().any(|t| t.contains("MEMORY")),
        "Cursor path has no memory hot card: {after_commit:?}"
    );
    assert!(
        !after_commit.iter().any(|t| t.contains("192.0.2.8")),
        "{after_commit:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn testhook_skill_injects_after_failed_tool() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join(".hyper-home");
    let skill_dir = home.join("skills").join("testhook");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: testhook\n---\nRerun the failing file only.\n",
    )
    .unwrap();
    let mut o = opts(&dir);
    o.home = Some(home);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("bash", json!({"command": "echo FAILED"})),
            turn_text("ok"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
    let out = agent.run("run the tests").await.unwrap();
    assert_eq!(out.text, "ok");
    let hidden = hidden_texts(&agent);
    assert!(
        !hidden.iter().any(|t| t.contains("Rerun the failing file")),
        "Cursor path does not auto-inject skills from tool output: {hidden:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn forced_skill_replaces_an_active_skill_note() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join(".hyper-home");
    for name in ["testhook", "modgen"] {
        let p = home.join("skills").join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("SKILL.md"),
            format!("---\nname: {name}\n---\nbody of {name}\n"),
        )
        .unwrap();
    }
    let mut o = opts(&dir);
    o.home = Some(home);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([
            turn_tool("bash", json!({"command": "echo FAILED"})),
            turn_text("hooked"),
            turn_text("switched"),
        ])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    agent.run("run the tests").await.unwrap();
    let after_fail = hidden_texts(&agent);
    assert!(
        !after_fail.iter().any(|t| t.contains("body of testhook")),
        "Cursor path does not auto-inject skills from FAILED: {after_fail:?}"
    );
    agent.run("[skill:modgen]\nemit pack").await.unwrap();
    let after = hidden_texts(&agent);
    assert!(
        after.iter().any(|t| t.contains("body of modgen")),
        "forced skill must inject immediately: {after:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn mcp_mounts_when_configured_and_injects_on_mention() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join(".grok-hyper")).unwrap();
    std::fs::write(
        dir.join(".grok-hyper").join("mcp.toml"),
        "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\nargs=[\"x.py\"]\ndescription=\"Lantern docs\"\nmethods=[\"search\"]\n",
    )
    .unwrap();
    let o = opts(&dir);
    let scripted = Scripted {
        turns: Mutex::new(VecDeque::from([turn_text("ok-docs"), turn_text("ok-mcp")])),
        meter: false,
    };
    let mut agent = Agent::new(scripted, o).unwrap();
    assert!(crate::tools_schema::has_tool(&agent.tools, "mcp"));
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(!sys.contains("mcp:"), "{sys}");
    assert!(!sys.contains("Lantern docs"), "{sys}");
    agent
        .run("Write docs/ARCHITECTURE.md then stop.")
        .await
        .unwrap();
    let after_docs = hidden_texts(&agent);
    assert!(
        !after_docs.iter().any(|t| t.contains("[mcp")),
        "{after_docs:?}"
    );
    agent
        .run("Use mcp with server docs and method search.")
        .await
        .unwrap();
    let after_mcp = hidden_texts(&agent);
    assert!(
        !after_mcp.iter().any(|t| t.contains("[mcp")),
        "Cursor path injects mcp cards only on an explicit [mcp:] prefix: {after_mcp:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn view_dispatch_attaches_when_caps_allow() {
    let dir = std::env::temp_dir().join(format!("grok-hyper-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        crate::media::PROBE_IMAGE_B64,
    )
    .unwrap();
    std::fs::write(dir.join("red.png"), png).unwrap();

    struct Seeing {
        inner: Scripted,
        caps: crate::media::MediaCaps,
    }
    impl Completer for Seeing {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            self.inner.complete(messages, tools).await
        }
        fn media_caps(&self) -> crate::media::MediaCaps {
            self.caps.clone()
        }
    }

    let mut caps = crate::media::MediaCaps::default();
    caps.image = Some(true);
    let seeing = Seeing {
        inner: Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("view", json!({"path": "red.png"})),
                turn_text("red"),
            ])),
            meter: false,
        },
        caps,
    };
    let mut agent = Agent::new(seeing, opts(&dir)).unwrap();
    assert!(crate::tools_schema::has_tool(&agent.tools, "view"));
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(!sys.contains("view(path)"));
    let out = agent.run("what color is red.png").await.unwrap();
    assert_eq!(out.text, "red");
    let viewed = agent
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("view tool message");
    assert!(
        viewed.text().contains("Image loaded: red.png"),
        "{}",
        viewed.text()
    );
    assert_eq!(viewed.parts.len(), 1);
    assert!(
        viewed.parts[0].url.contains(".grok-hyper/generated/")
            || viewed.parts[0].url.starts_with("data:image/png;base64,"),
        "live window should keep a disk path, not a multi-MB data URI: {}",
        viewed.parts[0].url
    );
    let _ = std::fs::remove_dir_all(dir);
}
