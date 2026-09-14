import { useEffect, useState } from "react";
import { api, failMsg, rpc, type Heartbeat } from "../api";
import { fmtAgo } from "../Chat";
import { Icon, PageHead, Switch, uiConfirm } from "../ui";

export function HeartbeatPage({ active = true, busy = false }: { active?: boolean; busy?: boolean }) {
  const [h, setH] = useState<Heartbeat>({ enabled: false, interval_s: 3600, prompt: "", last_run: null });
  const [resolved, setResolved] = useState("[heartbeat] Check workspace status. Reply with a short note.");
  const [saved, setSaved] = useState("");
  const [err, setErr] = useState("");
  useEffect(() => {
    if (!active) return;
    api<{ heartbeat: Heartbeat; resolved_prompt?: string }>("/heartbeat").then((j) => {
      setH((prev) => ({ ...prev, ...j.heartbeat }));
      if (j.resolved_prompt) setResolved(j.resolved_prompt);
    });
  }, [active]);
  const runNowPrompt = () => {
    const typed = (h.prompt || "").trim();
    if (typed) return `[heartbeat] ${typed}`;
    return resolved;
  };
  const runNow = async () => {
    if (busy) {
      const ok = await uiConfirm(
        "正在回复其他消息",
        "立即运行会按忙碌策略处理（默认在安全边界转向当前轮）。仍要现在运行？",
        { okLabel: "运行" },
      );
      if (!ok) return;
    }
    rpc("turn.start", { prompt: runNowPrompt() }).catch((e) => setErr(failMsg(e)));
  };
  return (
    <div className="page">
      <PageHead title="心跳" hint="HEARTBEAT.md" />
      <div className="page-body">
        <div className="card form-span">
          <div className="switch-row">
            <div>
              <b>启用</b>
              <div className="sub">打开后按间隔把 prompt（或 HEARTBEAT.md）当作用户消息发给模型</div>
            </div>
            <Switch checked={h.enabled} onChange={(v) => setH({ ...h, enabled: v })} label="启用心跳" />
          </div>
          <div className="field">
            <label>间隔（秒）</label>
            <input
              className="input mono"
              type="number"
              min={1}
              value={h.interval_s}
              onChange={(e) => setH({ ...h, interval_s: +e.target.value })}
            />
          </div>
          <div className="field">
            <label>请求内容（空则读工作区 HEARTBEAT.md）</label>
            <textarea className="input" rows={8} value={h.prompt} onChange={(e) => setH({ ...h, prompt: e.target.value })} />
          </div>
          <div className="sub" style={{ marginTop: 10 }}>
            上次运行 {fmtAgo(h.last_run)}
          </div>
          <div className="toolbar" style={{ marginTop: 14 }}>
            <button className="btn ghost" onClick={() => void runNow()}>
              <Icon name="play" />
              立即运行
            </button>
            <button
              className="btn primary"
              onClick={async () => {
                if (!Number.isFinite(h.interval_s) || h.interval_s < 1) {
                  setErr("间隔须为 ≥ 1 的秒数");
                  return;
                }
                setErr("");
                try {
                  await api("/heartbeat", { method: "POST", body: JSON.stringify(h) });
                  setSaved("已保存");
                } catch (e) {
                  setErr(failMsg(e));
                }
              }}
            >
              保存
            </button>
            {saved && !err ? <span className="sub">{saved}</span> : null}
            {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
          </div>
        </div>
      </div>
    </div>
  );
}