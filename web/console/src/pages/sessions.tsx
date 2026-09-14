import { useEffect, useState } from "react";
import { failMsg, rpc, sessionName, type SessionInfo } from "../api";
import { Empty, PageHead, uiAlert, uiConfirm, uiPrompt } from "../ui";
import { allShownPicked, filterSessions, sessionChannels, togglePicked } from "../sessions-model";

export function SessionsPage({
  current,
  running,
  onOpen,
  active = true,
}: {
  current?: string;
  running?: string[];
  onOpen: () => void;
  active?: boolean;
}) {
  const [rows, setRows] = useState<SessionInfo[]>([]);
  const [q, setQ] = useState("");
  const [channel, setChannel] = useState("all");
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const load = () => rpc<{ sessions?: SessionInfo[] }>("session.list", {}).then((j) => setRows(j.sessions || []));
  useEffect(() => {
    if (active) load();
    else setPicked(new Set());
  }, [current, active]);
  const channels = sessionChannels(rows);
  const shown = filterSessions(rows, q, channel);
  const shownIds = shown.map((s) => s.id);
  const allOn = allShownPicked(shownIds, picked);
  const runningSet = new Set(running || []);
  const togglePick = (id: string, on: boolean) => {
    setPicked((cur) => {
      return togglePicked(cur, id, on);
    });
  };
  const deletePicked = async (ids: string[]) => {
    if (ids.length === 0) return;
    const hit = ids.length === 1 ? shown.find((s) => s.id === ids[0]) || rows.find((s) => s.id === ids[0]) : undefined;
    const label =
      ids.length === 1 ? `删除会话「${hit ? sessionName(hit) : ids[0]}」？` : `删除 ${ids.length} 个会话？`;
    if (!(await uiConfirm(label, "会话记录与标题将一并删除，无法恢复。", { danger: true, okLabel: "删除" }))) return;
    await rpc("session.delete", ids.length === 1 ? { session: ids[0] } : { sessions: ids });
    setPicked(new Set());
    await load();
    if (current && ids.includes(current)) onOpen();
  };
  return (
    <div className="page">
      <PageHead title="会话" hint="所有频道" />
      <div className="page-body">
        <div className="toolbar">
          <input className="input inline" placeholder="按标题 / id / 预览筛选" value={q} onChange={(e) => setQ(e.target.value)} />
          <select className="input" style={{ width: 160 }} value={channel} onChange={(e) => setChannel(e.target.value)}>
            <option value="all">全部频道</option>
            {channels.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
          <span className="spacer" />
          <button
            type="button"
            className="btn danger small"
            disabled={picked.size === 0}
            onClick={() => void deletePicked([...picked])}
          >
            删除已选{picked.size > 0 ? ` ${picked.size}` : ""}
          </button>
        </div>
        <div className="card table-wrap" style={{ padding: 0 }}>
          {shown.length === 0 ? <Empty title="没有匹配的会话" body="换个关键词，或先去聊天里新建。" /> : null}
          {shown.length > 0 ? (
            <table>
              <thead>
                <tr>
                  <th className="tick">
                    <input
                      type="checkbox"
                      checked={allOn}
                      onChange={(e) => setPicked(e.target.checked ? new Set(shownIds) : new Set())}
                      aria-label="全选会话"
                    />
                  </th>
                  <th>标题</th>
                  <th>频道</th>
                  <th>模式</th>
                  <th>事件</th>
                  <th>预览</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {shown.map((s) => (
                  <tr key={s.id} className={s.id === current ? "sel" : undefined}>
                    <td className="tick">
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={(e) => togglePick(s.id, e.target.checked)}
                        aria-label={`选择 ${sessionName(s)}`}
                      />
                    </td>
                    <td>
                      <b className="session-name">
                        {runningSet.has(s.id) ? (
                          <span className="run-dot" title="运行中" aria-label="运行中" />
                        ) : null}
                        {sessionName(s)}
                      </b>
                      <div className="sub mono">{s.id}</div>
                    </td>
                    <td>{s.channel || "console"}</td>
                    <td className="mono">{s.mode}</td>
                    <td className="mono">{s.events ?? 0}</td>
                    <td>{(s.preview || "").slice(0, 72)}</td>
                    <td>
                      <div style={{ display: "flex", gap: 6 }}>
                        <button
                          className="btn ghost small"
                          onClick={async () => {
                            try {
                              await rpc("session.resume", { session: s.id });
                              onOpen();
                            } catch (e) {
                              void uiAlert("打开会话失败", failMsg(e));
                            }
                          }}
                        >
                          打开
                        </button>
                        <button
                          className="btn ghost small"
                          onClick={async () => {
                            const title = await uiPrompt("重命名会话", sessionName(s));
                            if (title) {
                              await rpc("session.title", { session: s.id, title });
                              await load();
                            }
                          }}
                        >
                          重命名
                        </button>
                        <button className="btn danger small" onClick={() => void deletePicked([s.id])}>
                          删除
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : null}
        </div>
      </div>
    </div>
  );
}
