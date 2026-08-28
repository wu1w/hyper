import { spawn } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { app, BrowserWindow, Menu, dialog, shell, ipcMain } from "electron";

const here = path.dirname(fileURLToPath(import.meta.url));
const READY_RE = /hyper web\s+(https?:\/\/\S+)/i;
const READY_MS = 20_000;

let backend = null;
let backendUrl = "";
let mainWindow = null;
let quitting = false;

function repoRoot() {
  return path.resolve(here, "../..");
}

function appIcon() {
  const packaged = path.join(process.resourcesPath, "icon.png");
  const dev = path.join(here, "build", "icon.png");
  if (app.isPackaged && existsSync(packaged)) return packaged;
  if (existsSync(dev)) return dev;
  return undefined;
}

function sidecarPaths() {
  if (app.isPackaged) {
    const binName = process.platform === "win32" ? "hyper.exe" : "hyper";
    return {
      bin: path.join(process.resourcesPath, "bin", binName),
      consoleDir: path.join(process.resourcesPath, "console"),
      vendorDir: path.join(process.resourcesPath, "vendor"),
    };
  }
  const root = repoRoot();
  const binName = process.platform === "win32" ? "hyper.exe" : "hyper";
  const release = path.join(root, "target", "release", binName);
  const debug = path.join(root, "target", "debug", binName);
  return {
    bin: existsSync(release) ? release : debug,
    consoleDir: path.join(root, "web", "console", "dist"),
    vendorDir: path.join(root, "third_party", "qwen-family"),
  };
}

function defaultWorkspace() {
  const dir = path.join(app.getPath("home"), ".grok-hyper", "workspace");
  mkdirSync(dir, { recursive: true });
  return dir;
}

function mergeNoProxy(env) {
  const extra = "127.0.0.1,localhost,::1";
  for (const key of ["NO_PROXY", "no_proxy"]) {
    const cur = (env[key] || "").trim();
    env[key] = cur ? `${extra},${cur}` : extra;
  }
}

function sidecarEnv(consoleDir, vendorDir) {
  const env = { ...process.env, HYPER_CONSOLE_DIR: consoleDir, HYPER_DESKTOP: "1" };
  if (vendorDir && existsSync(path.join(vendorDir, "qwen38", "chat_template.jinja"))) {
    env.HYPER_VENDOR_DIR = vendorDir;
  }
  const delim = path.delimiter;
  const home = app.getPath("home");
  const extras = (
    process.platform === "win32"
      ? [
          path.join(home, ".cargo", "bin"),
          path.join(home, ".local", "bin"),
          path.join(home, "go", "bin"),
          path.join(home, "scoop", "shims"),
          path.join(home, "AppData", "Roaming", "npm"),
          process.env.LOCALAPPDATA
            ? path.join(process.env.LOCALAPPDATA, "Microsoft", "WinGet", "Links")
            : "",
          process.env.ProgramFiles
            ? path.join(process.env.ProgramFiles, "Git", "cmd")
            : "C:\\Program Files\\Git\\cmd",
          process.env.ProgramFiles
            ? path.join(process.env.ProgramFiles, "nodejs")
            : "C:\\Program Files\\nodejs",
          process.env.ProgramData
            ? path.join(process.env.ProgramData, "chocolatey", "bin")
            : "C:\\ProgramData\\chocolatey\\bin",
        ]
      : [
          path.join(home, ".cargo", "bin"),
          path.join(home, ".local", "bin"),
          ...(process.platform === "darwin"
            ? [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/Applications/Docker.app/Contents/Resources/bin",
                path.join(home, ".docker", "bin"),
              ]
            : ["/usr/local/bin"]),
          "/usr/bin",
          "/bin",
        ]
  ).filter((dir) => dir && existsSync(dir));
  const current = env.PATH || env.Path || "";
  const parts = current.split(delim).filter(Boolean);
  const prepend = extras.filter((dir) => !parts.includes(dir));
  const merged = [...prepend, ...parts].join(delim);
  env.PATH = merged;
  if (process.platform === "win32") env.Path = merged;
  mergeNoProxy(env);
  return env;
}

function attachSidecarLog(child) {
  try {
    const dir = path.join(app.getPath("home"), ".grok-hyper");
    mkdirSync(dir, { recursive: true });
    const out = createWriteStream(path.join(dir, "desktop.log"), { flags: "w" });
    const MAX = 8 * 1024 * 1024;
    let written = 0;
    const write = (chunk) => {
      if (written >= MAX) return;
      try {
        written += chunk.length;
        out.write(chunk);
      } catch {
        /* ignore */
      }
    };
    child.stdout?.on("data", write);
    child.stderr?.on("data", write);
    child.once("exit", () => {
      try {
        out.end();
      } catch {
        /* ignore */
      }
    });
  } catch {
    /* logging is best-effort */
  }
}

function waitForUrl(child) {
  return new Promise((resolve, reject) => {
    let buf = "";
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      detach();
      reject(new Error(`hyper web 在 ${READY_MS / 1000}s 内没有打出监听地址`));
    }, READY_MS);

    // attachSidecarLog keeps the pipes drained for the process lifetime
    // (Windows anonymous pipes are ~4KB; a full pipe freezes hyper). This
    // waiter only scans until the ready line, then detaches so `buf` cannot
    // grow without bound.
    const detach = () => {
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
    };
    const onData = (chunk) => {
      const text = chunk.toString("utf8");
      if (settled) return;
      buf += text;
      if (buf.length > 64_000) buf = buf.slice(-32_000);
      const m = buf.match(READY_RE);
      if (!m) return;
      settled = true;
      clearTimeout(timer);
      detach();
      resolve(m[1].replace(/\/?$/, "/"));
    };
    const onExit = (code, signal) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      detach();
      reject(new Error(`hyper web 提前退出 (code=${code} signal=${signal})\n${buf.trim()}`));
    };
    const onError = (err) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      detach();
      reject(err);
    };

    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("error", onError);
    child.once("exit", onExit);
  });
}

async function startBackend() {
  if (backendUrl) return backendUrl;
  const { bin, consoleDir, vendorDir } = sidecarPaths();
  if (!existsSync(bin)) {
    throw new Error(`找不到 hyper 可执行文件: ${bin}`);
  }
  if (!existsSync(path.join(consoleDir, "index.html"))) {
    throw new Error(`找不到控制台 dist: ${consoleDir}`);
  }

  const child = spawn(bin, ["web", "--no-open", "--bind", "127.0.0.1:0"], {
    // Not the user profile: an empty/invalid [console] workspace falls back to
    // cwd, and indexing HOME on Windows blocks the first model call for minutes.
    cwd: defaultWorkspace(),
    env: sidecarEnv(consoleDir, vendorDir),
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  backend = child;
  attachSidecarLog(child);
  backendUrl = await waitForUrl(child);
  child.on("exit", () => {
    if (!quitting) {
      backend = null;
      backendUrl = "";
    }
  });
  return backendUrl;
}

function stopBackend() {
  if (!backend || backend.killed) return;
  const child = backend;
  backend = null;
  backendUrl = "";
  if (process.platform === "win32") {
    spawn("taskkill", ["/pid", String(child.pid), "/t", "/f"], {
      stdio: "ignore",
      windowsHide: true,
    });
    return;
  }
  child.kill("SIGINT");
  setTimeout(() => {
    if (!child.killed) child.kill("SIGKILL");
  }, 1500);
}

function installMenu() {
  const isMac = process.platform === "darwin";
  const template = [
    ...(isMac
      ? [
          {
            role: "appMenu",
          },
        ]
      : [
          {
            label: "文件",
            submenu: [{ role: "quit", label: "退出" }],
          },
        ]),
    { role: "editMenu" },
    { role: "viewMenu" },
    { role: "windowMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

async function createWindow() {
  let url;
  try {
    url = await startBackend();
  } catch (err) {
    dialog.showErrorBox("grok-hyper 无法启动", err instanceof Error ? err.message : String(err));
    app.quit();
    return;
  }

  const icon = appIcon();
  mainWindow = new BrowserWindow({
    width: 1280,
    height: 840,
    minWidth: 880,
    minHeight: 560,
    title: "grok-hyper",
    backgroundColor: "#111113",
    show: false,
    frame: false,
    autoHideMenuBar: true,
    ...(icon ? { icon } : {}),
    webPreferences: {
      preload: path.join(here, "preload.cjs"),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.once("ready-to-show", () => mainWindow?.show());
  mainWindow.on("closed", () => {
    mainWindow = null;
  });
  mainWindow.webContents.setWindowOpenHandler(({ url: target }) => {
    shell.openExternal(target);
    return { action: "deny" };
  });
  mainWindow.webContents.on("will-navigate", (event, target) => {
    if (target.startsWith(url)) return;
    event.preventDefault();
    shell.openExternal(target);
  });
  await mainWindow.loadURL(url);
}

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on("second-instance", () => {
    if (!mainWindow) return;
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.focus();
  });

  app.whenReady().then(async () => {
    app.setName("grok-hyper");
    installMenu();
    await createWindow();
    app.on("activate", async () => {
      if (BrowserWindow.getAllWindows().length === 0) await createWindow();
    });
  });

  app.on("window-all-closed", () => {
    if (process.platform !== "darwin") app.quit();
  });

  app.on("before-quit", () => {
    quitting = true;
    stopBackend();
  });
}

ipcMain.handle("desktop:pickFolder", async (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  const r = await dialog.showOpenDialog(win ?? undefined, {
    title: "选择工作区文件夹",
    properties: ["openDirectory", "createDirectory"],
  });
  if (r.canceled || !r.filePaths[0]) return { cancelled: true };
  return { path: r.filePaths[0] };
});
ipcMain.on("desktop:close", (event) => {
  BrowserWindow.fromWebContents(event.sender)?.close();
});
ipcMain.on("desktop:min", (event) => {
  BrowserWindow.fromWebContents(event.sender)?.minimize();
});
ipcMain.on("desktop:max", (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win) return;
  if (win.isMaximized()) win.unmaximize();
  else win.maximize();
});
