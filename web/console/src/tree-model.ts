export type TreeEntry = { path: string; name: string; dir: boolean };

export type TreeNode = TreeEntry & { children: TreeNode[] };

export function parseTreeEntry(raw: unknown): TreeEntry | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  const path = String(o.path ?? o.relative_path ?? o.rel ?? "")
    .replace(/\\/g, "/")
    .replace(/^\/+/, "");
  const name = String(o.name ?? o.file_name ?? o.filename ?? path.split("/").pop() ?? "");
  const dir = o.dir === true || o.is_dir === true || o.isDir === true || o.directory === true;
  const p = path || name;
  if (!p) return null;
  return { path: p, name: name || p.split("/").pop() || p, dir };
}

export function nestTree(entries: TreeEntry[]): TreeNode[] {
  const byPath = new Map<string, TreeNode>();
  for (const e of entries) {
    const path = e.path.replace(/\\/g, "/");
    byPath.set(path, { ...e, path, children: [] });
  }
  const root: TreeNode[] = [];
  for (const e of entries) {
    const path = e.path.replace(/\\/g, "/");
    const node = byPath.get(path);
    if (!node) continue;
    const i = path.lastIndexOf("/");
    if (i < 0) {
      root.push(node);
      continue;
    }
    const parent = byPath.get(path.slice(0, i));
    if (parent) parent.children.push(node);
    else root.push(node);
  }
  return root;
}

export function dirtyAncestors(paths: Iterable<string>): Set<string> {
  const out = new Set<string>();
  for (const raw of paths) {
    const p = raw.replace(/\\/g, "/").replace(/^\/+/, "");
    const parts = p.split("/").filter(Boolean);
    let acc = "";
    for (let i = 0; i < parts.length - 1; i++) {
      acc = acc ? `${acc}/${parts[i]}` : parts[i];
      out.add(acc);
    }
  }
  return out;
}
