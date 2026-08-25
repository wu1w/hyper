/** Copy into a standalone buffer. Views on a larger ArrayBuffer break JSZip / pdf.js / exceljs. */
export function copyU8(src: Uint8Array): Uint8Array {
  const out = new Uint8Array(src.byteLength);
  out.set(src);
  return out;
}

export function toArrayBuffer(src: Uint8Array): ArrayBuffer {
  return copyU8(src).buffer;
}

export function toBlob(src: Uint8Array, mime: string): Blob {
  return new Blob([copyU8(src)], { type: mime });
}

export function looksLikeZip(bytes: Uint8Array): boolean {
  return bytes.length >= 4 && bytes[0] === 0x50 && bytes[1] === 0x4b;
}

export function looksLikeOle(bytes: Uint8Array): boolean {
  return bytes.length >= 8 && bytes[0] === 0xd0 && bytes[1] === 0xcf && bytes[2] === 0x11 && bytes[3] === 0xe0;
}

export function looksLikePdf(bytes: Uint8Array): boolean {
  return bytes.length >= 5 && bytes[0] === 0x25 && bytes[1] === 0x50 && bytes[2] === 0x44 && bytes[3] === 0x46;
}

export function mimeFromPath(path: string): string {
  const n = (path.split(/[\\/]/).pop() || "").toLowerCase();
  const i = n.lastIndexOf(".");
  const ext = i >= 0 ? n.slice(i + 1) : "";
  switch (ext) {
    case "png":
      return "image/png";
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "gif":
      return "image/gif";
    case "webp":
      return "image/webp";
    case "svg":
      return "image/svg+xml";
    case "bmp":
      return "image/bmp";
    case "pdf":
      return "application/pdf";
    case "docx":
    case "docm":
      return "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    default:
      return "application/octet-stream";
  }
}
