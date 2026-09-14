import { useEffect, useRef, useState } from "react";
import { api, failMsg, rpc, type CronJob } from "../api";
import { fmtAgo } from "../Chat";
import { Empty, Icon, PageHead, Switch, uiConfirm } from "../ui";

export function CronPage({ active = true, busy = false }: { active?: boolean; busy?: boolean }) {
  const [jobs, setJobs] = useState<CronJob[]>([]);
  const [dirty, setDirty] = useState(false);
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const focusRef = useRef(false);
  const dirtyRef = useRef(false);
  dirtyRef.current = dirty;
  const load = () =>
    api<{ jobs: CronJob[] }>("/jobs").then((j) => {
      setJobs(j.jobs || []);
      setDirty(false);
    });
  useEffect(() => {
    if (!active) return;
    load();
    // 轮询只为刷新 last_run；正在编辑（聚焦或有未保存改动）时绝不覆盖表单。
    const id = window.setInterval(async () => {
      try {
        const j = await api<{ jobs: CronJob[] }>("/jobs");
        const server = j.jobs || [];
        if (focusRef.current || dirtyRef.current) {
          setJobs((cur) =>
            cur.map((x) => {
              const s = server.find((y) => y.id === x.id);
              return s && s.last_run !== x.last_run ? { ...x, last_run: s.last_run } : x;
            }),
          );
        } else setJobs(server);
      } catch {
        /* 轮询失败下次再试 */
      }
    }, 3000);
    return () => window.clearInterval(id);
  }, [active]);
  const edit = (i: number, patch: Partial<CronJob>) => {
    setJobs((cur) => {
      const n = [...cur];
      n[i] = { ...n[i], ...patch };
      return n;
    });
    setDirty(true);
    setMsg("");
  };
  const save = async () => {
    for (const x of jobs) {
      if (!Number.isFinite(x.interval_s) || x.interval_s < 1) {
        setErr(`任务「${x.name || x.id}」的间隔须为 ≥ 1 的秒数`);
        return;
      }
    }
    setErr("");
    try {
      await api("/jobs", { method: "POST", body: JSON.stringify({ jobs }) });
      setMsg("已保存");
      await load();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const addJob = () =>
    api("/jobs", {
      method: "POST",
      body: JSON.stringify({
        add: { id: crypto.randomUUID(), name: "新任务", interval_s: 3600, prompt: "", enabled: false },
      }),
    })
      .then(load)
      .catch((e) => setErr(failMsg(e)));
  const removeJob = async (x: CronJob) => {
    if (!(await uiConfirm(`删除定时任务「${x.name || x.id}」？`, undefined, { danger: true, okLabel: "删除" }))) return;
    api("/jobs", { method: "POST", body: JSON.stringify({ remove: x.id }) })
      .then(load)
      .catch((e) => setErr(failMsg(e)));
  };
  const runNow = async (x: CronJob) => {
    if (busy) {
      const ok = await uiConfirm(
        "正在回复其他消息",
        "立即运行会按忙碌策略处理（默认在安全边界转向当前轮）。仍要现在运行？",
        { okLabel: "运行" },
      );
      if (!ok) return;
    }
    rpc("turn.start", { prompt: `[cron:${x.name}] ${x.prompt}` }).catch((e) => setErr(failMsg(e)));
  };
  return (
    <div className="page">
      <PageHead
        title="定时任务"
        hint="间隔到点后当成一条用户消息发出"
        actions={
          <button className="btn primary small" onClick={() => addJob()}>
            <Icon name="plus" />
            创建任务
          </button>
        }
      />
      <div className="page-body">
        <div
          className="card"
          style={{ padding: "4px 0 2px" }}
          onFocusCapture={() => {
            focusRef.current = true;
          }}
          onBlurCapture={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) focusRef.current = false;
          }}
        >
          {jobs.length === 0 ? (
            <Empty title="还没有定时任务" body="在聊天里说「写个定时任务」，或点创建。间隔是秒，不是 crontab 表达式。任务写在工作区 .grok-hyper/cron.json。" />
          ) : null}
          {jobs.map((x, i) => (
            <div className="row wrap" key={x.id}>
              <Switch checked={!!x.enabled} onChange={(v) => edit(i, { enabled: v })} label={`启用 ${x.name}`} />
              <input
                className="input"
                style={{ width: 120 }}
                value={x.name}
                onChange={(e) => edit(i, { name: e.target.value })}
              />
              <input
                className="input mini mono"
                type="number"
                min={1}
                value={x.interval_s}
                onChange={(e) => edit(i, { interval_s: +e.target.value })}
              />
              <span className="suffix">秒</span>
              <input
                className="input"
                style={{ flex: 1 }}
                value={x.prompt}
                placeholder="发给模型的请求"
                onChange={(e) => edit(i, { prompt: e.target.value })}
              />
              <span className="mono sub" style={{ width: 88 }}>
                {fmtAgo(x.last_run)}
              </span>
              <button className="btn ghost small" title="立刻把这条任务发给模型" onClick={() => void runNow(x)}>
                <Icon name="play" />
                立即运行
              </button>
              <button className="btn danger small" onClick={() => void removeJob(x)}>
                删除
              </button>
            </div>
          ))}
        </div>
        <div className="toolbar" style={{ marginTop: 12 }}>
          <span className="sub">空闲时才触发。开关、名称、间隔、正文都要点「保存」才生效。</span>
          <span className="spacer" />
          {dirty ? <span className="pill warn">未保存修改</span> : null}
          {msg && !dirty ? <span className="sub">{msg}</span> : null}
          {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
          <button className="btn primary" disabled={!dirty} onClick={() => void save()}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}
