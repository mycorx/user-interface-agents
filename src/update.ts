// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

// Mirrors `uia_app::updater::UpdateStatus` (serde tag "status",
// snake_case). Duplicated across the IPC boundary like the settings types.
export type UpdateStatus =
  | { status: 'idle' }
  | { status: 'checking' }
  | { status: 'up_to_date'; current: string }
  | { status: 'available'; version: string; notes: string | null; error: string | null }
  | { status: 'downloading'; version: string; percent: number | null }
  | { status: 'failed'; message: string };

export type UpdateSnapshot = { current_version: string; status: UpdateStatus };

// The install gate's mirror, for greying the button out. Rust refuses an
// install in any other state regardless (`updater::may_install`).
export const QUIET_STATES = ['Idle', 'Listening'];

export function describeUpdate(s: UpdateStatus): string {
  switch (s.status) {
    case 'idle':
      return 'Not checked yet';
    case 'checking':
      return 'Checking…';
    case 'up_to_date':
      return `Up to date (${s.current})`;
    case 'available':
      return s.error
        ? `${s.version} available. The last install failed: ${s.error}`
        : `${s.version} available`;
    case 'downloading':
      return s.percent === null ? `Downloading ${s.version}…` : `Downloading ${s.version}… ${s.percent}%`;
    case 'failed':
      return s.message;
  }
}
