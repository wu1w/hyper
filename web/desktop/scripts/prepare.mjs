import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const root = path.resolve(desktop, "../..");
const logo = path.join(root, "web/console/src/assets/logo.png");
const iconPng = path.join(desktop, "build/icon.png");
const iconIco = path.join(desktop, "build/icon.ico");
const macBinSrc = path.join(root, "target/release/hyper");
const winCandidates = [
  path.join(root, "target/x86_64-pc-windows-msvc/release/hyper.exe"),
  path.join(root, "target/x86_64-pc-windows-gnu/release/hyper.exe"),
];

function die(msg) {
  console.error(msg);
  process.exit(1);
}

function sipsResize(src, dest, size) {
  if (process.platform !== "darwin") return false;
  const r = spawnSync("sips", ["-z", String(size), String(size), src, "--out", dest], {
    stdio: "inherit",
  });
  return r.status === 0 && existsSync(dest);
}

/** PNG-in-ICO (Vista+). electron-builder/rcedit need a real .ico, not a renamed PNG. */
function writeIcoFromPng(pngPath, icoPath) {
  const png = readFileSync(pngPath);
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(1, 4);
  const entry = Buffer.alloc(16);
  entry.writeUInt8(0, 0); // 256×256
  entry.writeUInt8(0, 1);
  entry.writeUInt8(0, 2);
  entry.writeUInt8(0, 3);
  entry.writeUInt16LE(1, 4);
  entry.writeUInt16LE(32, 6);
  entry.writeUInt32LE(png.length, 8);
  entry.writeUInt32LE(22, 12);
  writeFileSync(icoPath, Buffer.concat([header, entry, png]));
}

function copyIcon() {
  mkdirSync(path.join(desktop, "build"), { recursive: true });
  if (!sipsResize(logo, iconPng, 1024) && !existsSync(iconPng)) {
    copyFileSync(logo, iconPng);
  }
  const png256 = path.join(desktop, "build/icon-256.png");
  if (!sipsResize(iconPng, png256, 256)) {
    copyFileSync(iconPng, png256);
  }
  writeIcoFromPng(png256, iconIco);
}

function stageMac() {
  if (!existsSync(macBinSrc)) {
    die(`缺少 macOS hyper: ${macBinSrc}\n先执行 cargo build --release -p hyper-cli`);
  }
  const destDir = path.join(desktop, "resources/mac");
  rmSync(destDir, { recursive: true, force: true });
  mkdirSync(destDir, { recursive: true });
  const dest = path.join(destDir, "hyper");
  copyFileSync(macBinSrc, dest);
  chmodSync(dest, 0o755);
}

function stageWin() {
  const src = winCandidates.find((p) => existsSync(p));
  if (!src) return false;
  const destDir = path.join(desktop, "resources/win");
  rmSync(destDir, { recursive: true, force: true });
  mkdirSync(destDir, { recursive: true });
  copyFileSync(src, path.join(destDir, "hyper.exe"));
  return true;
}

const requireWin = process.argv.includes("--require-win");
copyIcon();
stageMac();
const win = stageWin();
if (requireWin && !win) {
  die(
    "缺少 Windows hyper.exe。先装 cargo-xwin（或 mingw-w64）再跑 scripts/build-sidecars.sh",
  );
}
console.log(win ? "staged mac + win sidecars" : "staged mac sidecar (no Windows hyper.exe yet)");
console.log(`icons: ${iconPng} ${iconIco}`);
