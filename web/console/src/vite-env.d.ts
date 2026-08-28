/// <reference types="vite/client" />

interface GrokHyperDesktop {
  platform: string;
  close: () => void;
  minimize: () => void;
  toggleMaximize: () => void;
  pickFolder?: () => Promise<{ cancelled?: boolean; path?: string }>;
}

interface Window {
  grokHyperDesktop?: GrokHyperDesktop;
}
