// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

export type Platform = 'windows' | 'macos' | 'linux' | 'other';

// Where the UA-CH / UA sniffing lives so every caller agrees on it. WebView2
// (Windows) and WKWebView / WebKitGTK (macOS / Linux) are the only webviews
// Tauri runs in here. UA-CH's `platform` is preferred where the webview
// offers it; the UA string is the fallback. No Rust command or os plugin is
// needed for this, which would mean a new dependency and a shared main.rs
// registration.
export function detectPlatform(): Platform {
  const uaData = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData;
  const platform = uaData?.platform ?? '';
  const ua = navigator.userAgent;

  if (platform ? platform === 'Windows' : /Windows/.test(ua)) return 'windows';
  if (platform ? platform === 'macOS' : /Mac OS X|Macintosh/.test(ua)) return 'macos';
  if (platform ? platform === 'Linux' : /Linux|X11/.test(ua)) return 'linux';
  return 'other';
}
