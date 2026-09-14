import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { Icon, PageHead, Seg } from "../ui";

export function SecurityPage({ active = true }: { active?: boolean }) {
  const [mode, setMode] = useState("ask");
  const [scope, setScope] = useState("workspace");
  const [msg, setMsg] = useState("");
  useEffect(() => {
    if (!active) return;
    api<{ features: { approvals: string; workspace_write_only: boolean } }>("/config")
      .then((j) => {
        const next = j.features?.approvals;
        if (next) setMode(next);
        setScope(j.features?.workspace_write_only === false ? "global" : "workspace");
      })
      .catch(() => {});
  }, [active]);
  return (
    <div className="page">
      <PageHead title="安全" hint="approvals" />
      <div className="page-body">
        <div className="card form-span">
          <h2>
            <Icon name="lock" />
            审批模式
          </h2>
          <div className="sub" style={{ margin: "8px 0 12px" }}>
            ask 逐一审批修改类工具 · auto 放行 write/edit · yolo 从不审批。计划模式走聊天 /plan。
          </div>
          <Seg
            value={mode}
            options={[
              { id: "ask", label: "ask" },
              { id: "auto", label: "auto" },
              { id: "yolo", label: "yolo" },
            ]}
            onChange={setMode}
          />
          <div className="sub" style={{ marginTop: 12 }}>
            门控：write · edit · bash · run_code · mcp · ComputerUse（点击/打字；截屏不审批）
          </div>
          <div className="setting-divider" />
          <h2>
            <Icon name="folder" />
            Agent 作用域
          </h2>
          <div className="sub" style={{ margin: "8px 0 12px" }}>
            工作区仅允许文件工具访问当前文件夹；全局允许使用绝对路径访问其他位置。
            终端与 Python 始终从工作区启动，但不是操作系统沙箱。
          </div>
          <Seg
            value={scope}
            options={[
              { id: "workspace", label: "工作区（推荐）" },
              { id: "global", label: "全局" },
            ]}
            onChange={setScope}
          />
          {scope === "global" ? (
            <div className="scope-warning">全局模式会扩大 Agent 可读取和修改的路径范围。</div>
          ) : null}
          <div style={{ display: "flex", alignItems: "center", marginTop: 16 }}>
            <button
              className="btn primary"
              onClick={async () => {
                try {
                  const saved = await api<{ agent_scope?: string }>("/config", {
                    method: "POST",
                    body: JSON.stringify({
                      approvals: mode,
                      workspace_write_only: scope === "workspace",
                    }),
                  });
                  if (saved.agent_scope !== scope) {
                    throw new Error("作用域未被后端应用，请重启 grok-hyper 服务后重试");
                  }
                  setMsg("已保存");
                } catch (e) {
                  setMsg(failMsg(e));
                }
              }}
            >
              保存
            </button>
            {msg ? <span className="sub" style={{ marginLeft: 10 }}>{msg}</span> : null}
          </div>
        </div>
      </div>
    </div>
  );
}
