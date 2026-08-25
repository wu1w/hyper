/// <reference types="vite/client" />

interface GrokHyperDesktop {
  platform: string;
  close: () => void;
  minimize: () => void;
  toggleMaximize: () => void;
}

interface Window {
  grokHyperDesktop?: GrokHyperDesktop;
}
