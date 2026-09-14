import { useEffect, useState } from "react";
import {
  api,
  usageCachePrompt,
  usageCachedReported,
  usageCompacts,
  usageHitPct,
  usageLivePrompt,
  usageSteps,
  type Snap,
  type Usage,
} from "../api";
import { Icon, PageHead } from "../ui";
import { PRICE_CLIFF } from "../settings-model";

export function UsagePage({ snap }: { snap: Snap }) {
  const [u, setU] = useState<Usage | null>(null);
  useEffect(() => {
    api<Usage>("/usage")
      .then(setU)
      .catch(() => setU(null));
  }, [snap.session, snap.usage?.assistant_steps, snap.usage?.assistant_steps]);
  const reported = usageCachedReported(u);
  const hitN = usageHitPct(u);
  const hit = reported && hitN != null ? hitN.toFixed(1) : null;
  const cached = u?.cached_tokens ?? 0;
  const prompt = u?.prompt_tokens ?? 0;
  const cachePrompt = usageCachePrompt(u) || prompt;
  const fresh = Math.max(0, cachePrompt - cached);
  const pct = cachePrompt ? Math.round((cached / cachePrompt) * 100) : 0;
  const live = usageLivePrompt(u) || prompt;
  const overCliff = live >= PRICE_CLIFF;
  return (
    <div className="page">
      <PageHead title="用量" hint="cached tokens · 200k 价格悬崖" />
      <div className="page-body">
        <div className={`banner warn${overCliff ? " hot" : ""}`}>
          200k 价格悬崖：输入超过 200k token 后单价翻倍 $2 / $0.50 / $6 → $4 / $1 / $12（每百万，输入 / 缓存命中 / 输出）。
          当前前缀 {live.toLocaleString()}
          {reported ? ` · 缓存命中 ${cached.toLocaleString()}` : " · 缓存命中未接入"}。
        </div>
        <div className="grid c3">
          <div className="card">
            <h2>
              <Icon name="chart" />
              prompt
            </h2>
            <div className="stat-num">{(u?.prompt_tokens ?? 0).toLocaleString()}</div>
            <div className="sub">
              当前前缀 {live.toLocaleString()}
              {" · "}completion {(u?.completion_tokens ?? 0).toLocaleString()}
              {usageCompacts(u) ? ` · compact ${usageCompacts(u)}` : ""}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="zap" />
              前缀命中
            </h2>
            <div className="stat-num">{hit ? `${hit}%` : "n/a"}</div>
            <div className="sub">
              first hop {(u?.first_hop_hit_rate ?? u?.first_hop_hit_rate) != null ? `${(Number(u?.first_hop_hit_rate ?? u?.first_hop_hit_rate) * 100).toFixed(1)}%` : "—"}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="cpu" />
              步数
            </h2>
            <div className="stat-num">{usageSteps(u)}</div>
            <div className="sub">window {snap.window || u?.window || "—"}</div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 12 }}>
          <h2>prompt 构成</h2>
          <div className="stack" style={{ marginTop: 12 }}>
            <div className="s1" style={{ width: `${pct}%` }} />
          </div>
          <div className="stack-legend">
            <span>
              <i style={{ background: "var(--brand)" }} />
              cached {reported ? cached.toLocaleString() : "n/a"}
            </span>
            <span>
              <i style={{ background: "var(--paper-3)" }} />
              其余 {fresh.toLocaleString()}
            </span>
          </div>
          <div className="sub" style={{ marginTop: 10 }}>
            stuck_first_hops = {u?.stuck_first_hops ?? u?.stuck_first_hops ?? 0}
            {(u?.prefix_note ?? u?.prefix_note) ? ` · ${u?.prefix_note ?? u?.prefix_note}` : ""}
          </div>
        </div>
      </div>
    </div>
  );
}
