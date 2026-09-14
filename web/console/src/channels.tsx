import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { api, failMsg, type ChannelEp, type ChannelKind } from "./api";
import {
  DM_POLICY,
  GROUP_POLICY,
  addableAction,
  addableKinds,
  addChannelPlan,
  cardStatus,
  channelCardClass,
  channelEnableHint,
  extraRecord,
  extraStr,
  isBound,
  kindSpec,
  matchLine,
  mergeQrCreds,
  mergeTags,
  newChannelRow,
  parseTagDraft,
  qrButtonLabel,
  qrFailMessage,
  qrPollDone,
  qrPollGap,
  qrStatusLabel,
  upsertBody,
} from "./channel-model";
import { Empty, PageHead, Seg, Switch, uiConfirm } from "./ui";

function ChannelTags({
  values,
  onChange,
  placeholder,
}: {
  values: string[];
  onChange: (v: string[]) => void;
  placeholder: string;
}) {
  const [draft, setDraft] = useState("");
  const add = (raw: string) => {
    const parts = parseTagDraft(raw);
    if (!parts.length) return;
    onChange(mergeTags(values, parts));
    setDraft("");
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(draft);
    }
    if (e.key === "Backspace" && !draft && values.length) onChange(values.slice(0, -1));
  };
  return (
    <div className="tag-list">
      {values.map((v) => (
        <span className="tag" key={v}>
          {v}
          <button type="button" aria-label={`移除 ${v}`} onClick={() => onChange(values.filter((x) => x !== v))}>
            ×
          </button>
        </span>
      ))}
      <input
        className="input tag-input"
        value={draft}
        placeholder={placeholder}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={onKey}
        onBlur={() => {
          if (draft.trim()) add(draft);
        }}
      />
    </div>
  );
}

function ChannelMark({ spec, sm }: { spec?: ChannelKind; sm?: boolean }) {
  const color = spec?.color || "#615CED";
  return (
    <span
      className={`ch-ico${sm ? " sm" : ""}`}
      data-kind={spec?.id || "webhook"}
      style={{ background: `${color}1f`, color }}
    >
      <span className="ch-mark">{spec?.mark || "?"}</span>
    </span>
  );
}

function QrBind({
  kind,
  domain,
  live,
  onBound,
}: {
  kind: string;
  domain?: string;
  live?: boolean;
  onBound: (creds: Record<string, string>) => void;
}) {
  const [image, setImage] = useState("");
  const [token, setToken] = useState("");
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const boundRef = useRef(onBound);
  boundRef.current = onBound;

  const start = async () => {
    setErr("");
    setBusy(true);
    setStatus("");
    try {
      const q = domain ? `?domain=${encodeURIComponent(domain)}` : "";
      const j = await api<{ image: string; poll_token: string }>(`/channels/${kind}/qrcode${q}`);
      setImage(j.image || "");
      setToken(j.poll_token || "");
      setStatus("waiting");
    } catch (e) {
      setErr(failMsg(e));
      setImage("");
      setToken("");
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (!token) return;
    let stop = false;
    let inFlight = false;
    const gap = qrPollGap(kind);
    const tick = async () => {
      if (stop || inFlight) return;
      inFlight = true;
      try {
        const q = new URLSearchParams({ token });
        if (domain) q.set("domain", domain);
        const j = await api<{ status: string; credentials?: Record<string, string> }>(
          `/channels/${kind}/qrcode/status?${q.toString()}`,
        );
        if (stop) return;
        setStatus(j.status);
        if (j.status === "success") {
          setToken("");
          boundRef.current(j.credentials || {});
        } else if (qrPollDone(j.status)) {
          setToken("");
          setErr(qrFailMessage(j.status, j.credentials?.fail_reason));
        }
      } catch (e) {
        if (!stop) setErr(failMsg(e));
      } finally {
        inFlight = false;
      }
    };
    const id = window.setInterval(tick, gap);
    tick();
    return () => {
      stop = true;
      window.clearInterval(id);
    };
  }, [token, kind, domain]);

  return (
    <div className="ch-qr">
      <div className="ch-qr-head">
        <div>
          <b>扫码绑定</b>
          <div className="sub">
            {live ? "用对应 App 扫码，绑定后本进程会连上" : "用对应 App 扫码完成绑定"}
          </div>
        </div>
        <button type="button" className="btn primary small" disabled={busy} onClick={start}>
          {qrButtonLabel(!!image)}
        </button>
      </div>
      {err ? <div className="err">{err}</div> : null}
      {image ? (
        <img className="ch-qr-img" src={image} alt={`${kind} 扫码绑定`} />
      ) : (
        <div className="ch-qr-ph">尚未取码。点「获取二维码」开始。</div>
      )}
      {status ? <div className={`ch-qr-st ${status}`}>{qrStatusLabel(status, kind, live)}</div> : null}
    </div>
  );
}

function ChannelGrid({
  rows,
  eps,
  sel,
  open,
  catalog,
  openAt,
}: {
  rows: ChannelEp[];
  eps: ChannelEp[];
  sel: number;
  open: boolean;
  catalog: ChannelKind[];
  openAt: (i: number) => void;
}) {
  return (
    <div className="ch-grid">
      {rows.map((e) => {
        const i = eps.findIndex((x) => x.id === e.id && x.kind === e.kind);
        return (
          <ChannelCard
            key={`${e.kind}-${e.id}`}
            e={e}
            catalog={catalog}
            selected={open && i === sel}
            onOpen={() => openAt(i)}
          />
        );
      })}
    </div>
  );
}

function ChannelCard({
  e,
  catalog,
  selected,
  onOpen,
}: {
  e: ChannelEp;
  catalog: ChannelKind[];
  selected: boolean;
  onOpen: () => void;
}) {
  const s = kindSpec(catalog, e.kind);
  const bound = isBound(e, s);
  const st = cardStatus(e);
  return (
    <button
      type="button"
      className={channelCardClass(selected, !!e.enabled)}
      onClick={onOpen}
    >
      <div className="ch-card-top">
        <ChannelMark spec={s} />
        <span className={`pill ${st.cls}`} title={e.runtime?.detail || undefined}>
          {st.label}
        </span>
      </div>
      <div className="ch-name">{s?.name || e.kind}</div>
      <div className="ch-id mono">{e.id}</div>
      <div className="ch-tags">
        {s?.qr ? <span className="pill ink">扫码</span> : null}
        {bound ? <span className="pill ok">{s?.in_process ? "已绑定" : "凭证已写入"}</span> : null}
        {s?.in_process ? <span className="pill idle">进程内</span> : null}
      </div>
      {(e.runtime?.state === "error" || e.runtime?.state === "retry") && e.runtime.detail ? (
        <div className="ch-err" title={e.runtime.detail}>
          {e.runtime.detail}
        </div>
      ) : null}
      <div className="sub">{matchLine(e)}</div>
      <div className="sub">{s?.blurb}</div>
    </button>
  );
}

export function ChannelsPage({ active = true }: { active?: boolean }) {
  const [busy, setBusy] = useState("steer");
  const [eps, setEps] = useState<ChannelEp[]>([]);
  const [catalog, setCatalog] = useState<ChannelKind[]>([]);
  const [sel, setSel] = useState(0);
  const [open, setOpen] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [err, setErr] = useState("");
  const load = (keepId?: string) =>
    api<{ busy: string; endpoints: ChannelEp[]; catalog?: ChannelKind[] }>("/channels").then((j) => {
      setBusy(j.busy);
      const rows = (j.endpoints || []).map((e) => ({ ...e, _origId: e.id }));
      setEps(rows);
      if (j.catalog?.length) setCatalog(j.catalog);
      if (keepId) {
        const i = rows.findIndex((e) => e.id === keepId);
        if (i >= 0) setSel(i);
      }
    });
  useEffect(() => {
    if (active && !open) load();
  }, [active, open]);
  const cur = eps[sel];
  const spec = cur ? kindSpec(catalog, cur.kind) : undefined;
  const enabled = eps.filter((e) => e.enabled);
  const idle = eps.filter((e) => !e.enabled);
  const configuredKinds = new Set(eps.map((e) => e.kind));
  const addable = addableKinds(catalog, eps);
  const patch = (p: Partial<ChannelEp>) => {
    if (!cur) return;
    const n = [...eps];
    n[sel] = { ...cur, ...p };
    setEps(n);
    setDirty(true);
  };
  const patchExtra = (key: string, value: string) => {
    if (!cur) return;
    const extra = { ...extraRecord(cur.extra), [key]: value };
    if (key === "bot_token") patch({ extra, bot_token: value });
    else patch({ extra });
  };
  const openAt = (i: number) => {
    setSel(i);
    setOpen(true);
    setDirty(false);
  };
  const add = (kind: string) => {
    const plan = addChannelPlan(kind, catalog, eps);
    if (plan.action === "ignore") return;
    if (plan.action === "open") {
      openAt(plan.index);
      return;
    }
    const row = newChannelRow(kind, eps);
    setEps([...eps, row]);
    setSel(eps.length);
    setOpen(true);
    setDirty(false);
  };
  /** 有未保存改动时先确认再关，防止点遮罩误丢配置。 */
  const closeDrawer = async () => {
    if (dirty) {
      const ok = await uiConfirm("放弃未保存的修改？", "这个频道刚才的改动还没保存。", {
        danger: true,
        okLabel: "放弃修改",
      });
      if (!ok) return;
    }
    setOpen(false);
    setDirty(false);
    await load();
  };
  useEffect(() => {
    if (!open) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") void closeDrawer();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // closeDrawer 闭包依赖 dirty/eps，最新一份即可
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, dirty]);
  const saveBusy = async (v: string) => {
    setBusy(v);
    try {
      await api("/channels", { method: "POST", body: JSON.stringify({ busy: v }) });
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const saveDrawer = async (row = cur, close = true) => {
    if (!row) return;
    try {
      setErr("");
      const body = upsertBody(row);
      await api("/channels", {
        method: "POST",
        body: JSON.stringify(body),
      });
      setDirty(false);
      if (close) {
        setOpen(false);
        await load();
      } else {
        await load(body.upsert.id);
      }
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const onQrBound = (creds: Record<string, string>) => {
    if (!cur) return;
    const row = mergeQrCreds(cur, creds);
    const n = [...eps];
    n[sel] = row;
    setEps(n);
    saveDrawer(row, false);
  };
  const removeCur = async () => {
    const id = (cur?._origId || cur?.id || "").trim();
    if (cur?._local || !id) {
      setSel(0);
      setOpen(false);
      setEps(eps.filter((_, i) => i !== sel));
      return;
    }
    const ok = await uiConfirm(`删除频道「${id}」？`, "已保存的凭证会一并删除；扫码平台需要重新扫码绑定。", {
      danger: true,
      okLabel: "删除",
    });
    if (!ok) return;
    setSel(0);
    setOpen(false);
    setDirty(false);
    try {
      setErr("");
      await api("/channels", { method: "POST", body: JSON.stringify({ remove: id }) });
      await load();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  return (
    <div className="page ch-page">
      <PageHead title="频道" hint="扫码或填字段绑定；进程内频道由本控制台自动连接" />
      <div className="page-body">
        <div className="toolbar">
          <span className="sub">忙碌时</span>
          <Seg
            value={busy}
            options={[
              { id: "interrupt", label: "打断" },
              { id: "queue", label: "排队" },
              { id: "steer", label: "转向" },
            ]}
            onChange={saveBusy}
          />
          <span className="spacer" />
          <span className="pill ink mono">cli</span>
          <span className="pill ink mono">sidecar</span>
          <span className="pill ink mono">console</span>
        </div>
        {err ? <div className="err">{err}</div> : null}

        <div className="ch-sec">
          <h3>
            已启用
            <span>{enabled.length}</span>
          </h3>
          {enabled.length > 0 ? (
            <ChannelGrid rows={enabled} eps={eps} sel={sel} open={open} catalog={catalog} openAt={openAt} />
          ) : (
            <div className="card">
              <Empty title="没有已启用的频道" body="从「可添加」选一个平台，扫码或填凭证后打开启用。" />
            </div>
          )}
        </div>

        {idle.length > 0 ? (
          <div className="ch-sec">
            <h3>
              未启用
              <span>{idle.length}</span>
            </h3>
            <ChannelGrid rows={idle} eps={eps} sel={sel} open={open} catalog={catalog} openAt={openAt} />
          </div>
        ) : null}

        <div className="ch-sec">
          <h3>
            可添加
            <span>绑定后由本进程自动连接</span>
          </h3>
          <div className="ch-grid avail">
            {addable.map((c) => (
              <button type="button" className="card ch-avail" key={c.id} onClick={() => add(c.id)}>
                <ChannelMark spec={c} sm />
                <span className="grow">
                  <b>{c.name}</b>
                  <div className="sub">{c.blurb}</div>
                </span>
                <span className="sub">{addableAction(c, configuredKinds.has(c.id))}</span>
              </button>
            ))}
          </div>
        </div>
      </div>

      {open && cur ? (
        <>
          <div className="drawer-mask" onClick={() => closeDrawer()} />
          <aside className="drawer ch-drawer" aria-label="频道配置">
            <header>
              <ChannelMark spec={spec} sm />
              <div className="grow min0">
                <b>{spec?.name || cur.kind}</b>
                <div className="sub">{cur.id}</div>
              </div>
              <button className="btn ghost small" onClick={() => closeDrawer()}>
                关闭
              </button>
            </header>
            <div className="drawer-body">
              <div className="switch-row">
                <div>
                  <b>已启用</b>
                  <div className="sub">{channelEnableHint(spec)}</div>
                </div>
                <Switch
                  checked={!!cur.enabled}
                  onChange={async (v) => {
                    if (v && spec && !spec.in_process) {
                      const ok = await uiConfirm(
                        "该平台适配器尚未进进程",
                        "凭证会保存进 config.toml，但本进程暂时不会实际连接、收发消息。仍要标记为启用？",
                        { okLabel: "仍然启用" },
                      );
                      if (!ok) return;
                    }
                    patch({ enabled: v });
                  }}
                  label="启用频道"
                />
              </div>
              <div className="field">
                <label>名称 · id</label>
                <input className="input mono" value={cur.id} onChange={(e) => patch({ id: e.target.value })} />
              </div>

              {spec?.qr ? (
                <QrBind
                  kind={cur.kind}
                  live={!!spec?.in_process}
                  domain={cur.kind === "feishu" ? extraStr(cur, "domain") || "feishu" : undefined}
                  onBound={onQrBound}
                />
              ) : null}

              {cur.kind === "feishu" ? (
                <div className="field">
                  <label>域名</label>
                  <Seg
                    value={extraStr(cur, "domain") || "feishu"}
                    options={[
                      { id: "feishu", label: "飞书" },
                      { id: "lark", label: "Lark" },
                    ]}
                    onChange={(domain) => patchExtra("domain", domain)}
                  />
                </div>
              ) : null}

              {(spec?.fields || [])
                .filter((f) => !(cur.kind === "feishu" && f.key === "domain"))
                .map((f) => {
                const saved = (cur.creds_set || []).includes(f.key) || (f.key === "bot_token" && cur.bot_token_set);
                return (
                  <div className="field" key={f.key}>
                    <label>
                      {f.label}
                      {saved ? "（已保存，留空不改）" : ""}
                    </label>
                    <input
                      className="input mono"
                      type={f.secret ? "password" : "text"}
                      value={f.key === "bot_token" ? cur.bot_token || extraStr(cur, f.key) : extraStr(cur, f.key)}
                      onChange={(e) => patchExtra(f.key, e.target.value)}
                      autoComplete="off"
                      placeholder={f.hint || ""}
                    />
                  </div>
                );
              })}

              {cur.kind === "webhook" ? (
                <div className="field">
                  <label>监听地址 · bind</label>
                  <input className="input mono" value={cur.bind || ""} onChange={(e) => patch({ bind: e.target.value })} />
                </div>
              ) : null}

              <div className="field">
                <label>出站 reply_url（可选）</label>
                <input className="input mono" value={cur.reply_url || ""} onChange={(e) => patch({ reply_url: e.target.value })} />
              </div>
              {cur.kind === "webhook" ? (
                <div className="field">
                  <label>secret · X-Q38-Token（留空保留）</label>
                  <input
                    className="input mono"
                    type="password"
                    value={cur.secret || ""}
                    onChange={(e) => patch({ secret: e.target.value })}
                    autoComplete="off"
                  />
                </div>
              ) : null}

              <div className="ch-block">
                <h4>匹配方式</h4>
                <p className="sub">
                  按 sender_id 过滤。默认私信白名单（配对码绑定）、群需 @。deny_from 优先拒绝；白名单非空时，即使策略是「开放」也只放行名单内。
                </p>
                <div className="field">
                  <label>私信 dm_policy</label>
                  <Seg value={cur.dm_policy || "allowlist"} options={DM_POLICY} onChange={(dm_policy) => patch({ dm_policy })} />
                </div>
                <div className="field">
                  <label>群聊 group_policy</label>
                  <Seg
                    value={cur.group_policy || "mention"}
                    options={GROUP_POLICY}
                    onChange={(group_policy) => patch({ group_policy })}
                  />
                </div>
                <div className="switch-row">
                  <div>
                    <b>群聊需 @提及</b>
                    <div className="sub">未 @ 的群消息直接丢弃</div>
                  </div>
                  <Switch
                    checked={!!cur.require_mention}
                    onChange={(v) => patch({ require_mention: v })}
                    label="群聊需提及"
                  />
                </div>
                <div className="field">
                  <label>白名单 allow_from</label>
                  <ChannelTags
                    values={cur.allow_from || []}
                    onChange={(allow_from) => patch({ allow_from })}
                    placeholder="sender_id，回车添加"
                  />
                </div>
                <div className="field">
                  <label>拒绝名单 deny_from</label>
                  <ChannelTags
                    values={cur.deny_from || []}
                    onChange={(deny_from) => patch({ deny_from })}
                    placeholder="sender_id，回车添加"
                  />
                </div>
              </div>
            </div>
            <footer>
              <button className="btn danger small" onClick={removeCur}>
                删除
              </button>
              <span className="spacer" />
              {dirty ? <span className="pill warn">未保存</span> : null}
              <button className="btn ghost small" onClick={() => closeDrawer()}>
                取消
              </button>
              <button className="btn primary small" disabled={!dirty && !cur._local} onClick={() => saveDrawer()}>
                保存
              </button>
            </footer>
          </aside>
        </>
      ) : null}
    </div>
  );
}
