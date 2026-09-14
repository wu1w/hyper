import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { PageHead } from "../ui";
import {
  authCards,
  normalizeAuthMode,
  parseEnvIgnored,
  parseTok,
  persistApplyBody,
  persistServerBody,
  pickAuthUrl,
  settingsFormError,
  showDeviceOverlay,
  windowPresetId,
  type AuthMode,
  type OauthState,
} from "../settings-model";
import { type WebCfg } from "../tools-model";
import {
  DeviceLoginOverlay,
  SettingsAuthPanel,
  SettingsBehaviorPanel,
  SettingsWebPanel,
  SettingsWindowPanel,
} from "./settings-panels";

export function SettingsPage({ active = true }: { active?: boolean }) {
  const [url, setUrl] = useState("");
  const [imageUrl, setImageUrl] = useState("");
  const [imageModel, setImageModel] = useState("");
  const [key, setKey] = useState("");
  const [keySet, setKeySet] = useState(false);
  const [model, setModel] = useState("");
  const [meta, setMeta] = useState("");
  const [lossy, setLossy] = useState(false);
  const [windowTok, setWindowTok] = useState("");
  const [maxSteps, setMaxSteps] = useState("");
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const [envIgnored, setEnvIgnored] = useState<string[]>([]);
  const [web, setWeb] = useState<WebCfg | null>(null);
  const [tavilyKey, setTavilyKey] = useState("");
  const [webMsg, setWebMsg] = useState("");
  const [probe, setProbe] = useState<{ busy: boolean; ok: boolean | null; model?: string; error?: string }>({
    busy: false,
    ok: null,
  });
  const [authMode, setAuthMode] = useState<AuthMode>("custom");
  const [wire, setWire] = useState<string>("");
  const [loggedIn, setLoggedIn] = useState<boolean | null>(null);
  const [sessionState, setSessionState] = useState("");
  const [oauth, setOauth] = useState<OauthState | null>(null);
  const [oauthBusy, setOauthBusy] = useState(false);
  const [family, setFamily] = useState("");
  const pickAuth = (mode: AuthMode) => {
    setAuthMode(mode);
    const next = pickAuthUrl(mode);
    if (next) setUrl(next);
  };
  const startOauth = async () => {
    setOauthBusy(true);
    setErr("");
    try {
      const j = await api<OauthState>("/auth/oauth", { method: "POST" });
      setOauth(j);
      if (j.authorize_url) window.open(j.authorize_url, "_blank", "noopener");
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setOauthBusy(false);
    }
  };
  const startDevice = async () => {
    setOauthBusy(true);
    setErr("");
    try {
      const j = await api<OauthState>("/auth/device", { method: "POST" });
      setOauth(j);
      const open = j.verification_uri_complete || j.verification_uri;
      if (open) window.open(open, "_blank", "noopener");
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setOauthBusy(false);
    }
  };
  const cancelOauth = async () => {
    try {
      await api("/auth/cancel", { method: "POST" });
    } catch {
      /* ignore */
    }
    setOauth(null);
  };
  const logoutSession = async () => {
    setOauthBusy(true);
    try {
      await api("/auth/logout", { method: "POST" });
      setMsg("已退出 grok login 会话");
      await load();
      await testConn();
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setOauthBusy(false);
    }
  };
  const load = () =>
    api<{
      auth_mode?: string;
      wire?: string;
      logged_in?: boolean;
      session?: string;
      server: {
        base_url: string;
        api_key: string;
        api_key_set?: boolean;
        model: string;
        image_base_url?: string;
        image_model?: string;
        image_model_resolved?: string;
        family: string;
        profile: string;
        auth_mode?: string;
        wire?: string;
        logged_in?: boolean;
      };
      policy: {
        low_precision?: boolean;
        max_steps?: number;
      };
      context?: { working_window?: number };
      env_ignored?: Record<string, boolean> | string[];
      web?: WebCfg;
    }>("/config").then((j) => {
      setUrl(j.server.base_url);
      setImageUrl(j.server.image_base_url || "");
      setImageModel(j.server.image_model || "");
      setModel(j.server.model);
      setKeySet(!!j.server.api_key_set);
      setFamily(j.server.family || "");
      setMeta(`${j.server.family || "未接入"} · ${j.server.profile}`);
      const url = j.server.base_url;
      setAuthMode(normalizeAuthMode(j.auth_mode || j.server.auth_mode, url));
      setWire(j.server.wire || j.wire || "");
      setLoggedIn(typeof (j.logged_in ?? j.server.logged_in) === "boolean" ? !!(j.logged_in ?? j.server.logged_in) : null);
      setSessionState(typeof j.session === "string" ? j.session : "");
      setLossy(!!j.policy.low_precision);
      setWindowTok(String(j.context?.working_window ?? ""));
      setMaxSteps(String(j.policy.max_steps ?? ""));
      setEnvIgnored(parseEnvIgnored(j.env_ignored));
      setWeb(j.web ?? null);
    });
  const testConn = async () => {
    setProbe((p) => ({ ...p, busy: true }));
    try {
      const j = await api<{ ok: boolean; model?: string; error?: string }>("/model");
      setProbe({ busy: false, ok: !!j.ok, model: j.model, error: j.error });
    } catch (e) {
      setProbe({ busy: false, ok: false, error: failMsg(e) });
    }
  };
  useEffect(() => {
    if (!active) return;
    load();
    testConn();
  }, [active]);
  useEffect(() => {
    if (oauth?.phase !== "waiting") return;
    let stop = false;
    const tick = async () => {
      try {
        const j = await api<OauthState>("/auth/oauth");
        if (stop) return;
        setOauth(j);
        if (j.phase === "ok") {
          setMsg("OAuth 登录成功");
          await load();
          await testConn();
        }
      } catch {
        /* ignore */
      }
    };
    const id = window.setInterval(() => void tick(), 1000);
    return () => {
      stop = true;
      window.clearInterval(id);
    };
  }, [oauth?.phase]);
  const persistServer = async () => {
    const formErr = settingsFormError({ authMode, url, imageUrl });
    if (formErr) {
      setErr(formErr);
      return;
    }
    setErr("");
    try {
      await api("/config", {
        method: "POST",
        body: JSON.stringify(persistServerBody({ authMode, url, key, model, imageUrl, imageModel })),
      });
      setKey("");
      setMsg("连接已写入 config.toml");
      await load();
      await testConn();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const saveWeb = async (patch: { web_enabled?: boolean; web_tavily_api_key?: string }) => {
    setWebMsg("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify(patch) });
      setTavilyKey("");
      setWebMsg("已保存");
      await load();
    } catch (e) {
      setWebMsg(failMsg(e));
    }
  };
  const parsedWindow = parseTok(windowTok);
  const presetId = windowPresetId(parsedWindow);
  return (
    <div className="page">
      <PageHead title="模型" hint="三路接入 · grok-4.6" />
      <div className="page-body">
        <div className="banner warn">
          200k 价格悬崖：输入超过 200k token 后单价翻倍 $2 / $0.50 / $6 → $4 / $1 / $12（每百万，输入 / 缓存命中 / 输出）。
          OAuth 会话、xAI API key、Grok 自建转发都走 Cursor 同款 POST /v1/responses（扁平 tools[]、function_call）。只有 Qwen / llama.cpp 网关才退回 Chat Completions。默认窗 500k。
        </div>
        {envIgnored.length > 0 ? (
          <div className="banner warn">
            检测到环境变量 {envIgnored.join("、")}。这些 HYPER_* 未被 hyper web 使用（BASE_URL / API_KEY / MODEL / WORKING_WINDOW 已生效）。
          </div>
        ) : null}
        <div className="auth-grid" role="radiogroup" aria-label="接入方式">
          {authCards({ sessionState, loggedIn, keySet, wire }).map((c) => (
            <button
              key={c.id}
              type="button"
              role="radio"
              aria-checked={authMode === c.id}
              className={`auth-card${authMode === c.id ? " on" : ""}`}
              onClick={() => pickAuth(c.id)}
            >
              <b>{c.title}</b>
              <span className="sub">{c.body}</span>
              <span className="sub">{c.extra}</span>
            </button>
          ))}
        </div>
        <div className="grid c2">
          <SettingsAuthPanel
            authMode={authMode}
            url={url}
            setUrl={setUrl}
            imageUrl={imageUrl}
            setImageUrl={setImageUrl}
            imageModel={imageModel}
            setImageModel={setImageModel}
            model={model}
            setModel={setModel}
            key={key}
            setKey={setKey}
            keySet={keySet}
            family={family}
            meta={meta}
            probe={probe}
            wire={wire}
            sessionState={sessionState}
            oauth={oauth}
            oauthBusy={oauthBusy}
            onTest={() => void testConn()}
            onOauth={() => void startOauth()}
            onDevice={() => void startDevice()}
            onLogout={() => void logoutSession()}
            onCancelOauth={() => void cancelOauth()}
            onSave={() => void persistServer()}
          />
          <SettingsBehaviorPanel lossy={lossy} setLossy={setLossy} maxSteps={maxSteps} setMaxSteps={setMaxSteps} />
        </div>
        <SettingsWindowPanel presetId={presetId} windowTok={windowTok} setWindowTok={setWindowTok} />
        {web ? (
          <SettingsWebPanel
            web={web}
            tavilyKey={tavilyKey}
            setTavilyKey={setTavilyKey}
            webMsg={webMsg}
            onSave={(patch) => void saveWeb(patch)}
          />
        ) : null}
        <div className="toolbar" style={{ marginTop: 12 }}>
          <button
            className="btn primary"
            onClick={async () => {
              setErr("");
              setMsg("");
              const formErr = settingsFormError({ authMode, url, imageUrl, windowTok, maxSteps, requireWindow: true });
              if (formErr) {
                setErr(formErr);
                return;
              }
              try {
                await api("/config", {
                  method: "POST",
                  body: JSON.stringify(
                    persistApplyBody({ authMode, url, key, model, imageUrl, imageModel, lossy, windowTok, maxSteps }),
                  ),
                });
                setKey("");
                setMsg("已写入 config.toml 并应用到当前会话");
                load();
              } catch (e) {
                setErr(String(e));
              }
            }}
          >
            保存并应用
          </button>
          {msg ? <span className="sub">{msg}</span> : null}
          {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
        </div>
      </div>
      {showDeviceOverlay(oauth) && oauth ? (
        <DeviceLoginOverlay oauth={oauth} onClose={() => void cancelOauth()} />
      ) : null}
    </div>
  );
}
