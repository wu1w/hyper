import { rpc } from "../api";

export type SaveResult = { path: string; bytes: number; sha256: string };

/** Overwrite a workspace file, then tell the session so the next hop Reads instead of replaying Write. */
export async function savePreview(path: string, data: Uint8Array, kind: string): Promise<SaveResult> {
  const put = await fetch(`/api/files?path=${encodeURIComponent(path)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/octet-stream" },
    body: data,
  });
  if (!put.ok) throw new Error(await put.text());
  const j = (await put.json()) as SaveResult & { ok?: boolean };
  await rpc("session.user_edit", {
    path: j.path || path,
    bytes: j.bytes,
    sha256: j.sha256,
    kind,
  });
  return { path: j.path || path, bytes: j.bytes, sha256: j.sha256 };
}

export async function loadPreviewBytes(path: string): Promise<{ bytes: Uint8Array; truncated: boolean }> {
  const r = await fetch(`/api/files?path=${encodeURIComponent(path)}`);
  if (!r.ok) throw new Error(await r.text());
  return {
    bytes: new Uint8Array(await r.arrayBuffer()),
    truncated: r.headers.get("x-hyper-truncated") === "1",
  };
}
