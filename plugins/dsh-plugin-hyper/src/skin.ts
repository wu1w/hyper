/** hyper alias-token skins. Applied by the dsh ui-theme presenter as body CSS variables. */

export type ColorScheme = 'light' | 'dark';

export type ThemeTokens = Record<string, string>;

export type SkinId = 'hyper-ink' | 'hyper-paper';

export type SkinDefinition = {
  id: SkinId;
  colorScheme: ColorScheme;
  label: string;
  tokens: ThemeTokens;
};

/** Dark: Grok void + Cursor zinc. White primary, no teal/purple. */
export const HYPER_INK: ThemeTokens = {
  '--dsw-alias-bg-base': '#050505',
  '--dsw-alias-bg-layer-1': '#0c0c0e',
  '--dsw-alias-bg-layer-2': '#141416',
  '--dsw-alias-bg-layer-3': '#1c1c1f',
  '--dsw-alias-bg-overlay': '#232326',
  '--dsw-alias-bg-module-platform': '#1c1c1f',
  '--dsw-alias-border-l1': 'rgba(255, 255, 255, 0.06)',
  '--dsw-alias-border-l2': 'rgba(255, 255, 255, 0.10)',
  '--dsw-alias-border-l3': 'rgba(255, 255, 255, 0.16)',
  '--dsw-alias-label-primary': '#f4f4f5',
  '--dsw-alias-label-secondary': '#a1a1aa',
  '--dsw-alias-label-tertiary': '#71717a',
  '--dsw-alias-label-caption': '#71717a',
  '--dsw-alias-label-dimmed': '#52525b',
  '--dsw-alias-brand-primary': '#f4f4f5',
  '--dsw-alias-brand-text': '#09090b',
  '--dsw-alias-button-primary-fill': '#f4f4f5',
  '--dsw-alias-button-primary-hover': '#ffffff',
  '--dsw-alias-button-info-fill': '#f4f4f5',
  '--dsw-alias-button-info-hover': '#ffffff',
  '--dsw-alias-state-business-primary': '#e4e4e7',
  '--dsw-alias-state-business-tertiary': '#27272a',
  '--dsw-alias-state-success-primary': '#4ade80',
  '--dsw-alias-state-error-primary': '#f87171',
  '--dsw-alias-state-warn-primary': '#eab308',
  '--dsw-specific-sidebar-fill': '#050505',
  '--dsw-specific-sidebar-nav-item-active': '#1c1c1f',
  '--dsw-specific-sidebar-nav-item-hover': '#121214',
  '--dsw-specific-sidebar-nav-item-active-accent': '#27272a',
  '--dsw-specific-bubble': '#18181b',
  '--dsw-specific-bubble-highlight': '#1f1f23',
  '--dsw-specific-input-major': '#18181b',
  '--dsw-specific-menu': '#1c1c1f',
  '--dsw-specific-selector': '#1c1c1f',
  '--dsw-specific-tip': '#18181b',
  '--dsw-alias-markdown-code-block': '#0a0a0c',
  '--dsw-alias-markdown-code-block-banner': '#121214',
  '--dsw-alias-markdown-inline-code': '#1c1c1f',
  '--dsw-alias-scrollbar-bg-l1': '#3f3f46',
  '--dsw-alias-scrollbar-hover-l1': '#52525b',
};

/** Light: Cursor cream page, black primary. */
export const HYPER_PAPER: ThemeTokens = {
  '--dsw-alias-bg-base': '#f7f7f4',
  '--dsw-alias-bg-layer-1': '#fbfbfa',
  '--dsw-alias-bg-layer-2': '#eeeeec',
  '--dsw-alias-bg-layer-3': '#e4e4e0',
  '--dsw-alias-bg-overlay': '#ffffff',
  '--dsw-alias-bg-module-platform': '#eeeeec',
  '--dsw-alias-border-l1': 'rgba(9, 9, 11, 0.08)',
  '--dsw-alias-border-l2': 'rgba(9, 9, 11, 0.14)',
  '--dsw-alias-border-l3': 'rgba(9, 9, 11, 0.2)',
  '--dsw-alias-label-primary': '#18181b',
  '--dsw-alias-label-secondary': '#52525b',
  '--dsw-alias-label-tertiary': '#71717a',
  '--dsw-alias-label-caption': '#71717a',
  '--dsw-alias-label-dimmed': '#a1a1aa',
  '--dsw-alias-brand-primary': '#18181b',
  '--dsw-alias-brand-text': '#18181b',
  '--dsw-alias-button-primary-fill': '#18181b',
  '--dsw-alias-button-primary-hover': '#09090b',
  '--dsw-alias-button-info-fill': '#18181b',
  '--dsw-alias-button-info-hover': '#09090b',
  '--dsw-alias-state-business-primary': '#18181b',
  '--dsw-alias-state-business-tertiary': '#e4e4e0',
  '--dsw-alias-state-success-primary': '#16a34a',
  '--dsw-alias-state-error-primary': '#dc2626',
  '--dsw-alias-state-warn-primary': '#ca8a04',
  '--dsw-specific-sidebar-fill': '#eeeeec',
  '--dsw-specific-sidebar-nav-item-active': '#e4e4e0',
  '--dsw-specific-sidebar-nav-item-hover': '#f4f4f2',
  '--dsw-specific-sidebar-nav-item-active-accent': '#d4d4d0',
  '--dsw-specific-bubble': '#eeeeec',
  '--dsw-specific-bubble-highlight': '#e4e4e0',
  '--dsw-specific-input-major': '#fbfbfa',
  '--dsw-specific-menu': '#eeeeec',
  '--dsw-specific-selector': '#eeeeec',
  '--dsw-specific-tip': '#f4f4f2',
  '--dsw-alias-markdown-code-block': '#f4f4f2',
  '--dsw-alias-markdown-code-block-banner': '#eeeeec',
  '--dsw-alias-markdown-inline-code': '#eeeeec',
  '--dsw-alias-scrollbar-bg-l1': '#d4d4d0',
  '--dsw-alias-scrollbar-hover-l1': '#a1a1aa',
};

export const SKINS: readonly SkinDefinition[] = [
  { id: 'hyper-ink', colorScheme: 'dark', label: 'hyper ink', tokens: HYPER_INK },
  { id: 'hyper-paper', colorScheme: 'light', label: 'hyper paper', tokens: HYPER_PAPER },
];

export const DEFAULT_SKIN: SkinId = 'hyper-ink';

export function cssVars(tokens: ThemeTokens): string {
  return Object.entries(tokens)
    .map(([k, v]) => `${k}: ${v};`)
    .join(' ');
}

/** Boot-time <style> so the first paint is hyper even if the client bundle is late. */
export function bootStyleTag(skin: SkinDefinition = SKINS[0]): string {
  const dark = skin.colorScheme === 'dark' ? ' data-ds-dark-theme="true"' : '';
  return `<style id="hyper-skin">${cssVars(skin.tokens)}</style><script>document.documentElement.setAttribute("data-hyper-skin","${skin.id}");</script><!-- hyper ${dark} -->`;
}

export function injectBootSkin(html: string, skin: SkinDefinition = SKINS[0]): string {
  const tag = bootStyleTag(skin);
  if (html.includes('id="hyper-skin"')) {
    return html;
  }
  if (html.includes('</head>')) {
    return html.replace('</head>', `${tag}</head>`);
  }
  return tag + html;
}
