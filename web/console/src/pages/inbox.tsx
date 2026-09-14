import { api, type Clarify, type Permit } from "../api";
import { Empty, Icon, PageHead } from "../ui";

export function InboxPage({
  permit,
  clarify = null,
  onPermit,
}: {
  permit: Permit;
  clarify?: Clarify;
  onPermit: (p: Permit) => void;
}) {
  const cur = permit;
  const go = async (d: string) => {
    if (!cur) return;
    await api("/permit", { method: "POST", body: JSON.stringify({ id: cur.id, decision: d }) });
    onPermit(null);
  };
  return (
    <div className="page">
      <PageHead title="收件箱" hint="AskQuestion 澄清 · 工具审批" />
      <div className="page-body">
        {clarify ? (
          <div className="banner warn" style={{ marginBottom: 12 }}>
            AskQuestion 待选择：{clarify.title || "请选择"} · 回聊天窗口点选项（模型在等你）。
          </div>
        ) : (
          <div className="sub" style={{ marginBottom: 12 }}>
            模型可用 AskQuestion 弹出 2–4 项选择题（开聊天栏的 AskQuestion）。写文件 / 跑命令的审批在本页。
          </div>
        )}
        <div className="split list-detail">
          <div className="card" style={{ padding: "6px 0" }}>
            <div style={{ padding: "8px 14px 10px" }}>
              <h2 style={{ margin: 0 }}>
                <Icon name="shield" />
                待决队列
              </h2>
              <div className="sub">一次只裁决队首。定时/心跳结果出现在聊天里。</div>
            </div>
            {cur ? (
              <div className="row on">
                <span className={`tc-badge ${cur.tool}`}>{cur.tool}</span>
                <div className="grow ellipsis">
                  <b className="mono" style={{ fontSize: 11.5 }}>
                    #{cur.id}
                  </b>
                  <div className="sub">{cur.preview.slice(0, 80)}</div>
                </div>
              </div>
            ) : (
              <Empty
                title="没有待审批项"
                body="ask 模式下 Write / StrReplace / Shell / mcp 会排到这里。Task 启动不需要批准；子代理里的写入仍走这扇门。AskQuestion 澄清题在聊天弹窗。"
              />
            )}
          </div>
          <div>
            {cur ? (
              <div className="card">
                <h2>
                  <Icon name="terminal" />#{cur.id} · {cur.tool}
                </h2>
                <div className="sub">裁决前该调用阻塞在 oneshot 上</div>
                <pre className="pre" style={{ margin: "12px 0" }}>
                  {cur.preview}
                </pre>
                <div style={{ display: "flex", gap: 8 }}>
                  <button className="btn primary" onClick={() => go("allow")}>
                    允许
                  </button>
                  <button className="btn ink" onClick={() => go("always")}>
                    始终允许
                  </button>
                  <button className="btn danger" onClick={() => go("deny")}>
                    拒绝
                  </button>
                </div>
              </div>
            ) : (
              <div className="card">
                <h2>
                  <Icon name="list" />
                  AskQuestion
                </h2>
                <div className="sub">
                  聊天栏打开 AskQuestion 后，模型可弹出选择题。审批模式在「安全」页切换。聊天弹窗与本页同步队首。
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
