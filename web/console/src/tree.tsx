import { useEffect, useMemo, useState } from "react";
import { Icon } from "./ui";
import {
  dirtyAncestors,
  nestTree,
  type TreeEntry,
  type TreeNode,
} from "./tree-model";

export type { TreeEntry, TreeNode } from "./tree-model";
export { parseTreeEntry, nestTree, dirtyAncestors } from "./tree-model";

function pathDirty(path: string, dirty: Set<string>): boolean {
  if (dirty.has(path)) return true;
  const prefix = `${path}/`;
  for (const d of dirty) {
    if (d.startsWith(prefix) || d === path) return true;
  }
  return false;
}

export function WorkspaceTree({
  entries,
  selected = "",
  dirty = [],
  onOpenFile,
}: {
  entries: TreeEntry[];
  selected?: string;
  dirty?: string[];
  onOpenFile: (path: string) => void;
}) {
  const nodes = useMemo(() => nestTree(entries), [entries]);
  const dirtySet = useMemo(() => new Set(dirty.map((p) => p.replace(/\\/g, "/"))), [dirty]);
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const seeded = useMemo(() => ({ current: false }), [entries]);

  useEffect(() => {
    setOpen((prev) => {
      const n = new Set(prev);
      if (!seeded.current && nodes.length) {
        for (const node of nodes) {
          if (node.dir) n.add(node.path);
        }
        seeded.current = true;
      }
      for (const a of dirtyAncestors(dirtySet)) n.add(a);
      return n;
    });
  }, [nodes, dirtySet, seeded]);

  const toggle = (path: string) => {
    setOpen((s) => {
      const n = new Set(s);
      if (n.has(path)) n.delete(path);
      else n.add(path);
      return n;
    });
  };

  if (!entries.length) {
    return <div className="sub ex-empty">工作区是空的，或树还没加载完。</div>;
  }

  return (
    <div className="ex-tree" role="tree">
      {nodes.map((node) => (
        <TreeRow
          key={node.path}
          node={node}
          depth={0}
          open={open}
          selected={selected}
          dirty={dirtySet}
          onToggle={toggle}
          onOpenFile={onOpenFile}
        />
      ))}
    </div>
  );
}

function TreeRow({
  node,
  depth,
  open,
  selected,
  dirty,
  onToggle,
  onOpenFile,
}: {
  node: TreeNode;
  depth: number;
  open: Set<string>;
  selected: string;
  dirty: Set<string>;
  onToggle: (path: string) => void;
  onOpenFile: (path: string) => void;
}) {
  const expanded = node.dir && open.has(node.path);
  const mark = pathDirty(node.path, dirty);
  return (
    <>
      <button
        type="button"
        role="treeitem"
        aria-expanded={node.dir ? expanded : undefined}
        className={`tree-row${selected === node.path ? " on" : ""}${mark ? " dirty" : ""}`}
        style={{ paddingLeft: 8 + depth * 12 }}
        onClick={() => {
          if (node.dir) onToggle(node.path);
          else onOpenFile(node.path);
        }}
      >
        {node.dir ? (
          <Icon name={expanded ? "chev-d" : "chev-r"} className="ico ex-chev" />
        ) : (
          <Icon name="file" />
        )}
        <span className="ellipsis">{node.name}</span>
      </button>
      {expanded
        ? node.children.map((kid) => (
            <TreeRow
              key={kid.path}
              node={kid}
              depth={depth + 1}
              open={open}
              selected={selected}
              dirty={dirty}
              onToggle={onToggle}
              onOpenFile={onOpenFile}
            />
          ))
        : null}
    </>
  );
}
