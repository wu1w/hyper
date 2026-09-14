import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { Empty, Icon, PageHead, Switch } from "../ui";
import { mcpTestText, parseEnvLines, splitMcpMethods, type McpServer } from "../mcp-model";

export function McpPage({ active = true }: { active?: boolean }) {
  const [data, setData] = useState<{
    auto_catalog?: boolean;
    servers?: McpServer[];
    editable?: McpServer[];
  }>({});
  const [draft, setDraft] = useState({ name: "", command: "", args: "", methods: "", description: "", env: "" });
  const [err, setErr] = useState("");
  const [created, setCreated] = useState("");
  const [testing, setTesting] = useState(false);
  const [testOut, setTestOut] = useState<{ ok: boolean; text: string } | null>(null);
  const load = () => api<typeof data>("/mcp").then(setData);
  useEffect(() => {
    if (active) load();
  }, [active]);
  const servers = data.servers || [];
  const editable = data.editable || [];
  const draftArgs = () => (draft.args.trim() ? draft.args.trim().split(/\s+/) : []);
  const testDraft = async () => {
    if (!draft.command.trim()) return;
    setTesting(true);
    setTestOut(null);
    try {
      const j = await api<{ ok: boolean; tools?: string[]; error?: string | null }>("/mcp/test", {
        method: "POST",
        body: JSON.stringify({
          command: draft.command.trim(),
          args: draftArgs(),
          env: parseEnvLines(draft.env),
        }),
      });
      setTestOut(mcpTestText(j.ok, j.tools, j.error));
    } catch (e) {
      setTestOut({ ok: false, text: `失败：${failMsg(e)}` });
    } finally {
      setTesting(false);
    }
  };
  return (
    <div className="page">
      <PageHead title="MCP" hint="一个 mcp() 工具 · stdio" />
      <div className="page-body">
        <div className="toolbar">
          <span className="sub">目录写入新会话 system</span>
          <Switch
            checked={!!data.auto_catalog}
            onChange={async (v) => {
              setErr("");
              try {
                await api("/mcp", { method: "POST", body: JSON.stringify({ auto_catalog: v }) });
                load();
              } catch (e) {
                setErr(failMsg(e));
              }
            }}
            label="mcp auto catalog"
          />
        </div>
        {err ? <div className="err">{err}</div> : null}
        <div className="split mcp">
          <div>
            <div className="card" style={{ padding: "6px 0 2px" }}>
              {servers.length === 0 ? <Empty title="没有服务器" body="右侧创建客户端。tools/list 不展开进冻结 tools[]。" /> : null}
              {servers.map((s) => (
                <div className="row" key={s.name}>
                  <div className="grow">
                    <b className="mono">{s.name}</b> <span className="sub mono">{s.command}</span>
                    <div className="sub">{s.description}</div>
                    <div style={{ marginTop: 6, display: "flex", gap: 5, flexWrap: "wrap" }}>
                      {(s.methods || []).map((m) => (
                        <span className="pill ink mono" key={m}>
                          {m}
                        </span>
                      ))}
                      {s.env_set || editable.some((x) => x.name === s.name && x.env_set) ? (
                        <span className="pill idle">env 已保存</span>
                      ) : null}
                      {s.editable === false ? (
                        <span className="pill idle">mcp.toml</span>
                      ) : null}
                    </div>
                  </div>
                  {editable.some((x) => x.name === s.name) ? (
                    <button
                      className="btn danger small"
                      onClick={() => {
                        setErr("");
                        api("/mcp", {
                          method: "POST",
                          body: JSON.stringify({ remove: s.name }),
                        })
                          .then(load)
                          .catch((e) => setErr(failMsg(e)));
                      }}
                    >
                      删除
                    </button>
                  ) : (
                    <span className="sub">在文件里删</span>
                  )}
                </div>
              ))}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="plus" />
              创建客户端
            </h2>
            <div className="sub">写入 config.toml 的 [mcp] servers；mcp.toml 里的条目不会被整表替换冲掉</div>
            <div className="field">
              <label>name</label>
              <input className="input mono" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
            </div>
            <div className="field">
              <label>command</label>
              <input className="input mono" value={draft.command} onChange={(e) => setDraft({ ...draft, command: e.target.value })} />
            </div>
            <div className="field">
              <label>args（空格分隔）</label>
              <input className="input mono" value={draft.args} onChange={(e) => setDraft({ ...draft, args: e.target.value })} />
            </div>
            <div className="field">
              <label>methods（可选，逗号分隔）</label>
              <input className="input mono" value={draft.methods} onChange={(e) => setDraft({ ...draft, methods: e.target.value })} />
            </div>
            <div className="field">
              <label>env（可选，每行一条 KEY=VALUE，如 API_KEY=xxx）</label>
              <textarea
                className="input mono"
                rows={3}
                value={draft.env}
                spellCheck={false}
                onChange={(e) => setDraft({ ...draft, env: e.target.value })}
              />
            </div>
            <div className="field">
              <label>description</label>
              <input className="input" value={draft.description} onChange={(e) => setDraft({ ...draft, description: e.target.value })} />
            </div>
            <div className="toolbar" style={{ margin: "14px 0 0" }}>
              <button
                className="btn ghost"
                disabled={!draft.command.trim() || testing}
                title="先起一次进程跑 initialize + tools/list，验证命令和 env 是否可用"
                onClick={() => void testDraft()}
              >
                {testing ? "测试中…" : "测试连通"}
              </button>
              <button
                className="btn primary"
                onClick={() => {
                  if (!draft.name.trim() || !draft.command.trim()) return;
                  setErr("");
                  setCreated("");
                  api("/mcp", {
                    method: "POST",
                    body: JSON.stringify({
                      add: {
                        name: draft.name.trim(),
                        command: draft.command.trim(),
                        args: draftArgs(),
                        env: parseEnvLines(draft.env),
                        methods: splitMcpMethods(draft.methods),
                        description: draft.description,
                      },
                    }),
                  })
                    .then(() => {
                      setCreated(`已创建「${draft.name.trim()}」。当前会话需发 /reload 或新建会话后才可调用。`);
                      setDraft({ name: "", command: "", args: "", methods: "", description: "", env: "" });
                      setTestOut(null);
                      load();
                    })
                    .catch((e) => setErr(failMsg(e)));
                }}
              >
                创建
              </button>
            </div>
            {testOut ? (
              <div className={testOut.ok ? "sub" : "err"} style={{ marginTop: 10, overflowWrap: "anywhere" }}>
                {testOut.text}
              </div>
            ) : null}
            {created ? <div className="sub" style={{ marginTop: 10 }}>{created}</div> : null}
          </div>
        </div>
      </div>
    </div>
  );
}
