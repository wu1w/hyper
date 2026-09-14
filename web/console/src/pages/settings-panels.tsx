import { Icon, Overlay, Seg, Switch } from "../ui";
import {
  IMAGE_MODELS,
  SESSION_URL,
  WINDOW_PRESETS,
  XAI_URL,
  chatOrigin,
  deviceOpenUrl,
  familyLine,
  imageEndpointHint,
  oauthWaiting,
  probeDotVar,
  probeStatusText,
  showOauthError,
  showOauthLink,
  type AuthMode,
  type OauthState,
} from "../settings-model";
import { webEngineLabel, type WebCfg } from "../tools-model";

type Probe = { busy: boolean; ok: boolean | null; model?: string; error?: string };

export function SettingsAuthPanel({
  authMode,
  url,
  setUrl,
  imageUrl,
  setImageUrl,
  imageModel,
  setImageModel,
  model,
  setModel,
  key,
  setKey,
  keySet,
  family,
  meta,
  probe,
  wire,
  sessionState,
  oauth,
  oauthBusy,
  onTest,
  onOauth,
  onDevice,
  onLogout,
  onCancelOauth,
  onSave,
}: {
  authMode: AuthMode;
  url: string;
  setUrl: (v: string) => void;
  imageUrl: string;
  setImageUrl: (v: string) => void;
  imageModel: string;
  setImageModel: (v: string) => void;
  model: string;
  setModel: (v: string) => void;
  key: string;
  setKey: (v: string) => void;
  keySet: boolean;
  family: string;
  meta: string;
  probe: Probe;
  wire: string;
  sessionState: string;
  oauth: OauthState | null;
  oauthBusy: boolean;
  onTest: () => void;
  onOauth: () => void;
  onDevice: () => void;
  onLogout: () => void;
  onCancelOauth: () => void;
  onSave: () => void;
}) {
  const waiting = oauthWaiting(oauth);
  return (
    <div className="card">
      <h2>
        <Icon name="cpu" />
        连接 · [server]
      </h2>
      <div className="row" style={{ padding: "8px 0", borderBottom: "none" }}>
        <span
          className="dot"
          style={{
            width: 8,
            height: 8,
            borderRadius: 999,
            flex: "0 0 8px",
            background: probeDotVar(probe.ok),
          }}
        />
        <span className="grow sub" style={{ fontSize: 12 }}>
          {probeStatusText(probe, wire)}
        </span>
        <button className="btn ghost small" disabled={probe.busy} onClick={onTest}>
          测试连接
        </button>
      </div>
      {probe.ok === false && probe.error ? (
        <div className="err" style={{ marginTop: 0, whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
          {probe.error}
        </div>
      ) : null}
      {authMode === "custom" ? (
        <div className="field">
          <label>base_url</label>
          <input
            className="input mono"
            value={url}
            placeholder="https://example.com/v1"
            onChange={(e) => setUrl(e.target.value)}
            spellCheck={false}
          />
        </div>
      ) : (
        <div className="sub" style={{ margin: "8px 0" }}>
          端点 {authMode === "session" ? SESSION_URL : XAI_URL}（不展示密钥）
        </div>
      )}
      <div className="field">
        <label>生图端点</label>
        <input
          className="input mono"
          value={imageUrl}
          placeholder={chatOrigin(authMode, url) || "https://api.x.ai/v1"}
          onChange={(e) => setImageUrl(e.target.value)}
          spellCheck={false}
        />
        <div className="sub" style={{ marginTop: 6 }}>
          {imageEndpointHint(authMode)}
        </div>
      </div>
      <div className="field">
        <label>生图 model</label>
        <input
          className="input mono"
          value={imageModel}
          list="image-model-presets"
          placeholder="grok-imagine-image-2.0"
          onChange={(e) => setImageModel(e.target.value)}
          spellCheck={false}
        />
        <datalist id="image-model-presets">
          {IMAGE_MODELS.map((id) => (
            <option key={id} value={id} />
          ))}
        </datalist>
        <div className="sub" style={{ marginTop: 6 }}>
          与对话 model 分开。可点选常见 Imagine 名，或手填网关自己的 id。留空则用 grok-imagine-image-2.0。
        </div>
      </div>
      {authMode === "session" ? (
        <div className="toolbar" style={{ margin: "0 0 10px", flexWrap: "wrap", gap: 8 }}>
          <button className="btn primary small" disabled={oauthBusy || waiting} onClick={onOauth}>
            浏览器 OAuth
          </button>
          <button className="btn ghost small" disabled={oauthBusy || waiting} onClick={onDevice}>
            设备码
          </button>
          {sessionState === "valid" ? (
            <button className="btn ghost small" disabled={oauthBusy} onClick={onLogout}>
              退出会话
            </button>
          ) : null}
          {waiting ? (
            <button className="btn ghost small" onClick={onCancelOauth}>
              取消登录
            </button>
          ) : null}
        </div>
      ) : null}
      {showOauthError(authMode, oauth) ? (
        <div className="err" style={{ marginTop: 0 }}>
          {oauth?.error}
        </div>
      ) : null}
      {showOauthLink(authMode, oauth) && oauth?.authorize_url ? (
        <div className="sub" style={{ margin: "0 0 10px" }}>
          已打开登录页。若被拦截，<a href={oauth.authorize_url} target="_blank" rel="noreferrer">点此完成 OAuth</a>。
        </div>
      ) : null}
      <div className="field">
        <label>model（占位 grok-4.6；空 = /v1/models 第一个）</label>
        <input
          className="input mono"
          value={model}
          placeholder="grok-4.6"
          onChange={(e) => setModel(e.target.value)}
          spellCheck={false}
        />
      </div>
      <div className="sub">{familyLine(family)}</div>
      {authMode !== "session" ? (
        <div className="field">
          <label>{keySet ? "api_key（已保存，留空不改）" : "api_key"}</label>
          <input
            className="input mono"
            type="password"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            autoComplete="off"
          />
        </div>
      ) : null}
      <div className="toolbar" style={{ margin: "12px 0 0" }}>
        <button className="btn primary small" onClick={onSave}>
          保存连接
        </button>
        <span className="sub">{meta}</span>
      </div>
      <div className="sub" style={{ marginTop: 8 }}>
        写入 ~/.grok-hyper/config.toml。OAuth 写入 ~/.grok/auth.json，密钥不出现在本页。
      </div>
    </div>
  );
}

export function SettingsBehaviorPanel({
  lossy,
  setLossy,
  maxSteps,
  setMaxSteps,
}: {
  lossy: boolean;
  setLossy: (v: boolean) => void;
  maxSteps: string;
  setMaxSteps: (v: string) => void;
}) {
  return (
    <div className="card">
      <h2>
        <Icon name="sliders" />
        行为
      </h2>
      <div className="switch-row">
        <div>
          <b>low_precision</b>
          <div className="sub">收紧 doom / 复读围栏。模型看不见。</div>
        </div>
        <Switch checked={lossy} onChange={setLossy} label="低精度" />
      </div>
      <div className="field">
        <label>max_steps · 每轮最大步数</label>
        <input
          className="input mono"
          inputMode="numeric"
          placeholder="500"
          value={maxSteps}
          onChange={(e) => setMaxSteps(e.target.value)}
          spellCheck={false}
        />
        <div className="sub" style={{ marginTop: 6 }}>
          手填整数。每轮工具循环上限，默认 500，与 IM 相同。范围 1–10000。下一轮生效。
        </div>
      </div>
      <div className="sub" style={{ marginTop: 10 }}>
        技能 / MCP 目录开关在各自页上，保存本页不会改它们。
      </div>
    </div>
  );
}

export function SettingsWindowPanel({
  presetId,
  windowTok,
  setWindowTok,
}: {
  presetId: string;
  windowTok: string;
  setWindowTok: (v: string) => void;
}) {
  return (
    <div className="card" style={{ marginTop: 12 }}>
      <h2>
        <Icon name="chart" />
        上下文窗口
      </h2>
      <div className="sub" style={{ margin: "4px 0 10px" }}>
        写入 [context] working_window。前缀超过窗宽×0.8（或 200k 价格悬崖）才 compact。单次生成长度由服务端自行决定。
      </div>
      <Seg
        value={presetId}
        options={[
          ...WINDOW_PRESETS.map((p) => ({ id: p.id, label: p.label })),
          { id: "custom", label: "自定义" },
        ]}
        onChange={(id) => {
          const p = WINDOW_PRESETS.find((x) => x.id === id);
          if (p) setWindowTok(String(p.n));
        }}
      />
      <div className="field">
        <label>working_window（token，可点 8k / 32k / 128k / 256k / 500k）</label>
        <input
          className="input mono"
          value={windowTok}
          onChange={(e) => setWindowTok(e.target.value)}
          spellCheck={false}
        />
      </div>
    </div>
  );
}

export function SettingsWebPanel({
  web,
  tavilyKey,
  setTavilyKey,
  webMsg,
  onSave,
}: {
  web: WebCfg;
  tavilyKey: string;
  setTavilyKey: (v: string) => void;
  webMsg: string;
  onSave: (patch: { web_enabled?: boolean; web_tavily_api_key?: string }) => void;
}) {
  return (
    <div className="card" style={{ marginTop: 12 }}>
      <h2>
        <Icon name="globe" />
        联网搜索 · [web]
      </h2>
      <div className="switch-row">
        <div>
          <b>启用 web 工具</b>
          <div className="sub">
            当前引擎：{webEngineLabel(web)}。
            填 Tavily key 可自动升级搜索质量，失败时回退内置引擎。
          </div>
        </div>
        <Switch checked={!!web.enabled} onChange={(v) => onSave({ web_enabled: v })} label="启用 web 工具" />
      </div>
      <div className="field">
        <label>{web.tavily_key_set ? "Tavily API key（已保存，留空不改）" : "Tavily API key（可选）"}</label>
        <div style={{ display: "flex", gap: 8 }}>
          <input
            className="input mono"
            type="password"
            value={tavilyKey}
            placeholder="tvly-…"
            onChange={(e) => setTavilyKey(e.target.value)}
            autoComplete="off"
          />
          <button
            className="btn ghost small"
            style={{ flex: "0 0 auto", height: 31 }}
            disabled={!tavilyKey.trim()}
            onClick={() => onSave({ web_tavily_api_key: tavilyKey.trim() })}
          >
            保存 key
          </button>
        </div>
      </div>
      {webMsg ? <div className="sub" style={{ marginTop: 8 }}>{webMsg}</div> : null}
    </div>
  );
}

export function DeviceLoginOverlay({
  oauth,
  onClose,
}: {
  oauth: OauthState;
  onClose: () => void;
}) {
  const href = deviceOpenUrl(oauth);
  return (
    <Overlay onClose={onClose}>
      <div className="modal dialog" role="dialog" aria-modal="true" aria-label="设备码登录">
        <h2>设备码登录</h2>
        <div className="sub" style={{ margin: "2px 0 8px", lineHeight: 1.6 }}>
          在任意已登录浏览器打开链接，输入下面的代码。
        </div>
        <div className="mono" style={{ fontSize: 22, letterSpacing: "0.12em", margin: "8px 0 12px" }}>
          {oauth.user_code}
        </div>
        {oauth.verification_uri ? (
          <a href={href} target="_blank" rel="noreferrer">
            {oauth.verification_uri}
          </a>
        ) : null}
        <div className="m-actions" style={{ marginTop: 16 }}>
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
        </div>
      </div>
    </Overlay>
  );
}
