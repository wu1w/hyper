import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { Empty, Icon, PageHead, Switch } from "../ui";

export function SkillsPage({ active = true }: { active?: boolean }) {
  const [data, setData] = useState<{
    auto_catalog?: boolean;
    skills?: Array<{ name: string; description: string; path: string }>;
  }>({});
  const [q, setQ] = useState("");
  const [sel, setSel] = useState<string | null>(null);
  const [err, setErr] = useState("");
  const load = () => api<typeof data>("/skills").then(setData);
  useEffect(() => {
    if (active) load();
  }, [active]);
  const skills = (data.skills || []).filter((s) => `${s.name} ${s.description}`.toLowerCase().includes(q.toLowerCase()));
  const picked = skills.find((s) => s.name === sel) || skills[0];
  return (
    <div className="page">
      <PageHead title="技能" hint="SKILL.md · 不进 tools[]" />
      <div className="page-body">
        <div className="toolbar">
          <input className="input inline" placeholder="搜索技能名 / 描述" value={q} onChange={(e) => setQ(e.target.value)} />
          <span className="spacer" />
          <span className="sub">目录写入新会话 system</span>
          <Switch
            checked={!!data.auto_catalog}
            onChange={async (v) => {
              setErr("");
              try {
                await api("/skills", { method: "POST", body: JSON.stringify({ auto_catalog: v }) });
                load();
              } catch (e) {
                setErr(failMsg(e));
              }
            }}
            label="auto catalog"
          />
        </div>
        {err ? <div className="err">{err}</div> : null}
        <div className="split list-detail">
          <div className="grid">
            {skills.length === 0 ? (
              <div className="card">
                <Empty title="没有技能" body="在 ~/.grok-hyper/skills 或工作区 .grok-hyper/skills 放 SKILL.md，会话里 /技能名 触发。" />
              </div>
            ) : null}
            {skills.map((s) => (
              <button
                key={s.name}
                type="button"
                className="card"
                style={{ textAlign: "left", borderColor: picked?.name === s.name ? "var(--brand)" : undefined }}
                onClick={() => setSel(s.name)}
              >
                <h2>
                  <Icon name="spark" />
                  {s.name}
                </h2>
                <div className="sub">{s.description || "无描述"}</div>
              </button>
            ))}
          </div>
          {picked ? (
            <div className="card">
              <h2>{picked.name}</h2>
              <p>{picked.description}</p>
              <div className="sub mono">{picked.path}</div>
              <div className="sub" style={{ marginTop: 12 }}>
                技能体以隐藏用户卡注入（≤400 tok）。改 SKILL.md 后下一轮自动加载。auto catalog 只写入之后新建的会话。hyper 没有逐卡启用开关。
              </div>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}
