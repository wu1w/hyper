import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { Icon, PageHead, Switch } from "../ui";
import { CURSOR_TOOLS, onOffPill, webEnabledOf, webProviderOf, type WebCfg } from "../tools-model";

export function ToolsPage({ active = true }: { active?: boolean }) {
  const [j, setJ] = useState<{
    note?: string;
    frozen?: Array<{ function?: { name: string; description: string; parameters?: unknown } }>;
    view?: { function?: { name: string; description: string } };
    computer?: { function?: { name: string; description: string } };
    computer_enabled?: boolean;
    code_search_enabled?: boolean;
    media_enabled?: boolean;
    skill?: { function?: { name: string; description: string } };
    mcp?: { function?: { name: string; description: string } };
    web?: { function?: { name?: string; description?: string } };
  }>({});
  const [webCfg, setWebCfg] = useState<WebCfg | null>(null);
  const [computerOn, setComputerOn] = useState(false);
  const [searchOn, setSearchOn] = useState(false);
  const [computerMsg, setComputerMsg] = useState("");
  const [searchMsg, setSearchMsg] = useState("");
  useEffect(() => {
    if (!active) return;
    api<typeof j>("/tools").then((t) => {
      setJ(t);
      if (typeof t.computer_enabled === "boolean") setComputerOn(t.computer_enabled);
      if (typeof t.code_search_enabled === "boolean") setSearchOn(t.code_search_enabled);
    });
    api<{ web?: WebCfg; features?: { computer_use?: boolean; code_search?: boolean } }>("/config")
      .then((c) => {
        setWebCfg(c.web ?? null);
        if (typeof c.features?.computer_use === "boolean") setComputerOn(c.features.computer_use);
        if (typeof c.features?.code_search === "boolean") setSearchOn(c.features.code_search);
      })
      .catch(() => setWebCfg(null));
  }, [active]);
  const saveComputer = async (v: boolean) => {
    setComputerMsg("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify({ computer_use: v }) });
      setComputerOn(v);
      setComputerMsg("已保存，下一轮对话生效");
    } catch (e) {
      setComputerMsg(failMsg(e));
    }
  };
  const saveSearch = async (v: boolean) => {
    setSearchMsg("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify({ code_search: v }) });
      setSearchOn(v);
      setSearchMsg("已保存，下一轮对话生效");
    } catch (e) {
      setSearchMsg(failMsg(e));
    }
  };
  const frozen = j.frozen || [];
  // 实际引擎以 tools 描述里的 "provider: xxx" 为准（它考虑了 env / MCP 里的 key），config 兜底。
  const webDesc = j.web?.function?.description || "";
  const webProvider = webProviderOf(webDesc, webCfg);
  const webEnabled = webEnabledOf(webCfg, !!j.web);
  return (
    <div className="page">
      <PageHead title="工具" hint="冻结 tools[] · Cursor 名" />
      <div className="page-body">
        <p className="sub">
          发给模型的冻结名是 Cursor 的：{CURSOR_TOOLS}。下列来自后端；若仍显示 read/bash，说明 loop 尚未切换。
        </p>
        <p className="sub">{j.note}</p>
        <div className="card" style={{ padding: "4px 0 2px", marginTop: 8 }}>
          {frozen.map((t, i) => (
            <div className="row" key={t.function?.name}>
              <b className="mono" style={{ width: 26, color: "var(--label-3)" }}>
                {i + 1}
              </b>
              <span className="mono" style={{ width: 128, fontWeight: 700 }}>
                {t.function?.name}
              </span>
              <span className="grow">{t.function?.description}</span>
              <span className="pill ok">冻结</span>
            </div>
          ))}
        </div>
        <div className="grid c3" style={{ marginTop: 12 }}>
          <div className="card">
            <h2>
              <Icon name="file" />
              view
            </h2>
            <div className="sub">{j.view?.function?.description || "媒体预览"}</div>
            <span className="pill idle" style={{ marginTop: 8, display: "inline-block" }}>
              media.enabled 才挂
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="spark" />
              skill
            </h2>
            <div className="sub">{j.skill?.function?.description || "按名加载 SKILL.md"}</div>
            <span className="pill idle" style={{ marginTop: 8, display: "inline-block" }}>
              不进 tools[]
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="plug" />
              mcp
            </h2>
            <div className="sub">{j.mcp?.function?.description || "MCP 调用"}</div>
              <span className="pill idle" style={{ marginTop: 8, display: "inline-block" }}>
              有服务器时追加
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="command" />
              ComputerUse
            </h2>
            <div className="sub">
              {j.computer?.function?.description ||
                "截屏并控制本机键鼠（Windows / macOS）。坐标以最近一张截图为准。"}
            </div>
            <div className="switch-row" style={{ marginTop: 8 }}>
              <span className={`pill ${onOffPill(computerOn).cls}`}>
                {onOffPill(computerOn).label}
              </span>
              <Switch
                checked={computerOn}
                onChange={(v) => void saveComputer(v)}
                label="启用 ComputerUse"
              />
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              在运行 hyper 的这台电脑上执行，不是浏览器里。macOS 需「屏幕录制」+「辅助功能」；点击/打字在 ask/auto 下会审批。Task 子代理看不到也不会执行。下一轮对话生效。
            </div>
            {computerMsg ? <div className="sub" style={{ marginTop: 6 }}>{computerMsg}</div> : null}
          </div>
          <div className="card">
            <h2>
              <Icon name="search" />
              Search
            </h2>
            <div className="sub">工作区 FTS。冻结 17 里已有 Grep / Glob / Read，默认不挂。</div>
            <div className="switch-row" style={{ marginTop: 8 }}>
              <span className={`pill ${onOffPill(searchOn).cls}`}>
                {onOffPill(searchOn).label}
              </span>
              <Switch
                checked={searchOn}
                onChange={(v) => void saveSearch(v)}
                label="启用 Search"
              />
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              打开后下一轮 tools[] 在冻结 17 后面追加 Search。grok-4.6 的 Cursor 分布是 Grep。
            </div>
            {searchMsg ? <div className="sub" style={{ marginTop: 6 }}>{searchMsg}</div> : null}
          </div>
          <div className="card">
            <h2>
              <Icon name="globe" />
              web
            </h2>
            <div className="sub">{webDesc || "联网搜索与抓页（query 搜索 / url 抓正文）"}</div>
            <div style={{ marginTop: 8, display: "flex", gap: 6, flexWrap: "wrap" }}>
              <span className={`pill ${onOffPill(webEnabled).cls}`}>{onOffPill(webEnabled).label}</span>
              {webEnabled ? <span className="pill ink mono">{webProvider}</span> : null}
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              在「模型」页配置开关与 Tavily key。
            </div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 12 }}>
          <h2>
            <Icon name="shield" />
            内建护栏
          </h2>
          <div className="sub" style={{ marginBottom: 4 }}>
            始终在线，无需配置。触发时会以「注记」出现在聊天轨迹里。
          </div>
          <ul className="sub" style={{ margin: "6px 0 0", paddingLeft: 18, lineHeight: 1.9 }}>
            <li>盲覆写保护：Write 一个本会话没读过的已有文件会被拒绝，先 Read 再写。</li>
            <li>复读围栏：同一工具同参数连打先提醒、再停机，防死循环烧 token。</li>
            <li>审批门控：ask 模式下 Write / StrReplace / Shell / mcp / ComputerUse 点击打字逐一批准。截屏不弹窗。Task 本身不弹窗，子代理里的写入仍会。ComputerUse 只给父代理。</li>
            <li>工作区边界：文件工具只能操作当前文件夹内路径。</li>
          </ul>
        </div>
        <div className="sub" style={{ marginTop: 12 }}>
          tools[] 顺序和字节冻结，不能逐项开关。名字应对齐 Cursor，而不是 Qwen 的 read/bash。
        </div>
      </div>
    </div>
  );
}