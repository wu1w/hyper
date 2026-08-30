import { spawn, type ChildProcess } from "node:child_process";
import { createInterface, type Interface as ReadlineInterface } from "node:readline";

export type JsonRpcId = number | string;

export type SessionEvent = {
  type: string;
  text?: string;
  content?: string;
  reasoning?: string;
  channel?: string;
  delta?: boolean;
  reset?: boolean;
  name?: string;
  output?: string;
  reason?: string;
  tool_call_id?: string;
  tool_calls?: Array<{
    id?: string;
    function?: { name?: string; arguments?: unknown };
  }>;
  [key: string]: unknown;
};

export type EventHandler = (event: SessionEvent) => void;

export type EditorFile = {
  path: string;
  line?: number;
  selection?: string;
};

export type EditorContext = {
  active?: string;
  files: EditorFile[];
};

type Pending = {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
};

export type SpawnOptions = {
  workspace: string;
  session: string;
  command?: string;
  onStderr?: (line: string) => void;
};

/**
 * Dumb pipe to `hyper --sidecar`. Does not execute tools.
 */
export class HyperSidecar {
  private readonly child: ChildProcess;
  private readonly rl: ReadlineInterface;
  private readonly pending = new Map<JsonRpcId, Pending>();
  private readonly listeners = new Set<EventHandler>();
  private nextId = 1;
  private closed = false;

  private constructor(child: ChildProcess, onStderr?: (line: string) => void) {
    this.child = child;
    const stdout = child.stdout;
    if (!stdout) {
      throw new Error("hyper sidecar requires piped stdout");
    }
    this.rl = createInterface({ input: stdout, crlfDelay: Infinity });
    this.rl.on("line", (line) => this.onLine(line));
    this.rl.on("close", () => this.failAll(new Error("hyper sidecar stdout closed")));
    if (child.stderr && onStderr) {
      const errRl = createInterface({ input: child.stderr, crlfDelay: Infinity });
      errRl.on("line", onStderr);
    }
    child.on("error", (err) => this.failAll(err));
    child.on("exit", (code, signal) => {
      if (this.closed) {
        return;
      }
      this.failAll(
        new Error(
          signal
            ? `hyper sidecar killed by ${signal}`
            : `hyper sidecar exited with code ${code ?? "unknown"}`,
        ),
      );
    });
  }

  static spawn(opts: SpawnOptions): HyperSidecar {
    const child = spawn(
      opts.command ?? "hyper",
      ["--sidecar", "--workspace", opts.workspace, "--session", opts.session],
      { stdio: ["pipe", "pipe", "pipe"], cwd: opts.workspace },
    );
    if (!child.stdin || !child.stdout) {
      throw new Error("hyper sidecar requires piped stdin and stdout");
    }
    return new HyperSidecar(child, opts.onStderr);
  }

  onEvent(handler: EventHandler): () => void {
    this.listeners.add(handler);
    return () => {
      this.listeners.delete(handler);
    };
  }

  sessionOpen(params: {
    session: string;
    workspace: string;
    mode?: string;
  }): Promise<{ ok: true; session?: string }> {
    return this.call("session.open", params) as Promise<{ ok: true; session?: string }>;
  }

  slash(text: string): Promise<{ ok: true }> {
    return this.call("slash", { text }) as Promise<{ ok: true }>;
  }

  turnStart(
    prompt: string,
    editor?: EditorContext,
  ): Promise<{ ok: true; queued?: boolean; n?: number }> {
    const params: Record<string, unknown> = { prompt };
    if (editor) {
      params.editor = editor;
    }
    return this.call("turn.start", params) as Promise<{
      ok: true;
      queued?: boolean;
      n?: number;
    }>;
  }

  turnAbort(): Promise<{ ok: true }> {
    return this.call("turn.abort", {}) as Promise<{ ok: true }>;
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    try {
      await this.turnAbort();
    } catch {
      /* process may already be gone */
    }
    this.rl.close();
    if (this.child.stdin?.writable) {
      this.child.stdin.end();
    }
    this.child.kill();
    this.failAll(new Error("hyper sidecar closed"));
  }

  call(method: string, params: unknown = {}): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error("hyper sidecar closed"));
    }
    const id = this.nextId++;
    const req = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      const stdin = this.child.stdin;
      if (!stdin) {
        this.pending.delete(id);
        reject(new Error("hyper sidecar stdin is not writable"));
        return;
      }
      stdin.write(`${JSON.stringify(req)}\n`, (err) => {
        if (err) {
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  private onLine(line: string): void {
    const trimmed = line.trim();
    if (!trimmed) {
      return;
    }
    let msg: { id?: JsonRpcId; method?: string; params?: unknown; result?: unknown; error?: { code: number; message: string } };
    try {
      msg = JSON.parse(trimmed) as typeof msg;
    } catch {
      return;
    }
    if (msg.method === "event.append" && msg.params && typeof msg.params === "object") {
      const event = msg.params as SessionEvent;
      for (const handler of this.listeners) {
        handler(event);
      }
      return;
    }
    if (msg.id === undefined || msg.id === null) {
      return;
    }
    const pending = this.pending.get(msg.id);
    if (!pending) {
      return;
    }
    this.pending.delete(msg.id);
    if (msg.error) {
      pending.reject(new Error(msg.error.message));
    } else {
      pending.resolve(msg.result);
    }
  }

  private failAll(err: Error): void {
    for (const pending of this.pending.values()) {
      pending.reject(err);
    }
    this.pending.clear();
  }
}
