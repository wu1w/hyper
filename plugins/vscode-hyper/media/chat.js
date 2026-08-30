const vscode = acquireVsCodeApi();
const log = document.getElementById("log");
const input = document.getElementById("input");
const send = document.getElementById("send");
const abort = document.getElementById("abort");
const status = document.getElementById("status");

let thinkEl = null;
let thinkBuf = "";
let answerEl = null;
let liveRaf = 0;
let pendingAnswer = "";
let liveAnswer = "";

const WRITE_TOOL_NAME =
  /"name"\s*:\s*"(Write|StrReplace|Delete|write|str_replace|strreplace|delete|Edit)"/;

function stripLeakedToolJson(think) {
  if (!think) return think;
  let out = "";
  let i = 0;
  while (i < think.length) {
    const start = think.indexOf("```", i);
    if (start < 0) {
      const tail = think.slice(i);
      const trimmed = tail.trimEnd();
      const nl = trimmed.lastIndexOf("\n");
      const lineAt = nl < 0 ? 0 : nl + 1;
      const line = trimmed.slice(lineAt).trimStart();
      if ((line.startsWith("{") || line.startsWith("[")) && WRITE_TOOL_NAME.test(line)) {
        out += think.slice(i, i + lineAt);
      } else {
        out += tail;
      }
      break;
    }
    out += think.slice(i, start);
    const after = start + 3;
    const nl = think.indexOf("\n", after);
    if (nl < 0) break;
    const lang = think.slice(after, nl).trim();
    const close = think.indexOf("```", nl + 1);
    if (close < 0) break;
    const inner = think.slice(nl + 1, close).trim();
    const jsonish = !lang || /^json$/i.test(lang);
    if (jsonish && WRITE_TOOL_NAME.test(inner) && (inner.startsWith("{") || inner.startsWith("["))) {
      i = close + 3;
      continue;
    }
    out += think.slice(start, close + 3);
    i = close + 3;
  }
  return out;
}

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text) n.textContent = text;
  return n;
}

function add(node) {
  log.appendChild(node);
  log.scrollTop = log.scrollHeight;
  return node;
}

function ensureThink() {
  if (!thinkEl) thinkEl = add(el("div", "think"));
  return thinkEl;
}

function ensureAnswer() {
  if (!answerEl) {
    const wrap = el("div", "msg assistant");
    wrap.appendChild(el("div", "who", "Hyper"));
    answerEl = el("div", "bubble");
    wrap.appendChild(answerEl);
    add(wrap);
  }
  return answerEl;
}

function cancelLive() {
  if (!liveRaf) return;
  cancelAnimationFrame(liveRaf);
  liveRaf = 0;
}

let imeLock = false;
let busy = false;
abort.disabled = true;

function imeBusy(e) {
  return imeLock || e.isComposing === true || e.keyCode === 229;
}

function setBusy(v) {
  busy = !!v;
  abort.disabled = !busy;
  input.placeholder = busy
    ? "本轮结束后会跑这段话… Enter 排队"
    : "给 Hyper 发消息… Enter 发送，Shift+Enter 换行";
}

function splitFences(src) {
  const out = [];
  let i = 0;
  while (i < src.length) {
    const open = src.indexOf("```", i);
    if (open < 0) {
      if (i < src.length) out.push({ k: "t", s: src.slice(i) });
      break;
    }
    if (open > i) out.push({ k: "t", s: src.slice(i, open) });
    const nl = src.indexOf("\n", open + 3);
    if (nl < 0) {
      out.push({ k: "code", s: "" });
      break;
    }
    const close = src.indexOf("\n```", nl + 1);
    if (close < 0) {
      out.push({ k: "code", s: src.slice(nl + 1) });
      break;
    }
    out.push({ k: "code", s: src.slice(nl + 1, close) });
    i = close + 4;
    if (src[i] === "\n") i += 1;
  }
  return out;
}

function paintFences(node, text) {
  node.replaceChildren();
  for (const p of splitFences(text)) {
    if (p.k === "t") {
      if (!p.s) continue;
      const span = document.createElement("span");
      span.className = "md-t";
      span.textContent = p.s;
      node.appendChild(span);
    } else {
      const pre = document.createElement("pre");
      pre.className = "md-pre";
      const code = document.createElement("code");
      code.textContent = p.s;
      pre.appendChild(code);
      node.appendChild(pre);
    }
  }
}

function flushLive() {
  liveRaf = 0;
  if (thinkBuf) {
    ensureThink().textContent = stripLeakedToolJson(thinkBuf).trimEnd();
  }
  if (pendingAnswer) {
    liveAnswer += pendingAnswer;
    pendingAnswer = "";
    paintFences(ensureAnswer(), liveAnswer);
  }
  log.scrollTop = log.scrollHeight;
}

function scheduleLive() {
  if (liveRaf) return;
  liveRaf = requestAnimationFrame(flushLive);
}

function finishThink() {
  if (thinkEl) thinkEl.classList.add("think-done");
  thinkEl = null;
}

function resetLive() {
  cancelLive();
  thinkBuf = "";
  pendingAnswer = "";
  liveAnswer = "";
  finishThink();
  answerEl = null;
}

send.addEventListener("click", () => {
  const text = input.value;
  if (!text.trim()) return;
  vscode.postMessage({ type: "send", text });
  input.value = "";
});
abort.addEventListener("click", () => vscode.postMessage({ type: "abort" }));
input.addEventListener("compositionstart", () => {
  imeLock = true;
});
input.addEventListener("compositionend", () => {
  imeLock = true;
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      imeLock = false;
    });
  });
});
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    if (imeBusy(e)) return;
    e.preventDefault();
    send.click();
  }
});

window.addEventListener("message", (ev) => {
  const msg = ev.data || {};
  if (msg.type === "reset") {
    log.innerHTML = "";
    resetLive();
    return;
  }
  if (msg.type === "status") {
    status.textContent = msg.text || "";
    if ("busy" in msg) setBusy(!!msg.busy);
    return;
  }
  if (msg.type === "ready") {
    status.textContent = msg.session ? `就绪 · ${msg.session}` : "就绪";
    setBusy(false);
    return;
  }
    if (msg.type === "error") {
    flushLive();
    resetLive();
    add(el("div", "err", msg.text || "error"));
    setBusy(false);
    return;
  }
  if (msg.type === "user") {
    flushLive();
    resetLive();
    const wrap = el("div", "msg user");
    wrap.appendChild(el("div", "who", "你"));
    wrap.appendChild(el("div", "bubble", msg.text || ""));
    add(wrap);
    return;
  }
  if (msg.type !== "event" || !msg.event) return;
  const e = msg.event;
  if (e.type === "delta") {
    if (e.reset) {
      cancelLive();
      pendingAnswer = "";
      liveAnswer = "";
      if (e.content_only) {
        if (answerEl) answerEl.replaceChildren();
      } else {
        thinkBuf = "";
        if (thinkEl) thinkEl.textContent = "";
        if (answerEl) answerEl.replaceChildren();
      }
    }
    if (e.channel === "reasoning") {
      thinkBuf += e.text || "";
      scheduleLive();
    } else if (e.channel === "content") {
      pendingAnswer += e.text || "";
      scheduleLive();
    }
    return;
  }
  if (e.type === "assistant") {
    flushLive();
    const calls = e.tool_calls || [];
    if (calls.length) {
      for (const c of calls) {
        const name = (c.function && c.function.name) || "tool";
        let path = "";
        try {
          const raw = c.function && c.function.arguments;
          const a = typeof raw === "string" ? JSON.parse(raw || "{}") : raw || {};
          path = a.path || "";
        } catch (_) {}
        const btn = el("button", "tool", path ? `${name}  ${path}` : name);
        if (path) {
          btn.addEventListener("click", () => vscode.postMessage({ type: "open", path }));
        }
        add(btn);
      }
      finishThink();
      answerEl = null;
      liveAnswer = "";
    } else if (e.content) {
      liveAnswer = e.content;
      paintFences(ensureAnswer(), e.content);
    }
    return;
  }
  if (e.type === "stop") {
    flushLive();
    finishThink();
    answerEl = null;
    liveAnswer = "";
  }
});
