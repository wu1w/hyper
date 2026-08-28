const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("grokHyperDesktop", {
  platform: process.platform,
  close: () => ipcRenderer.send("desktop:close"),
  minimize: () => ipcRenderer.send("desktop:min"),
  toggleMaximize: () => ipcRenderer.send("desktop:max"),
  pickFolder: () => ipcRenderer.invoke("desktop:pickFolder"),
});
