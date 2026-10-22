// Google Calendar import: the shared status and the auto-refresh timer (FR-12.5).
// Nothing runs until an account is connected in Settings.
import { api, eventWriteHooks, type GcalStatus } from './api';
import { data } from './state.svelte';

export const gcal = $state({ status: null as GcalStatus | null, syncing: false });

/** Matches MIN_INTERVAL_MIN in src-tauri/src/gcal.rs. */
export const MIN_INTERVAL_MIN = 5;
export const INTERVALS = [5, 15, 30, 60, 180];

let timer: ReturnType<typeof setInterval> | undefined;

/** With two-way sync on, an edit is sent to Google a few seconds later. */
const PUSH_DELAY_MS = 4000;
let pushTimer: ReturnType<typeof setTimeout> | undefined;
/** An edit arrived while a sync was running; sync again when it ends. */
let pushAgain = false;

function schedulePush() {
  if (!gcal.status?.connected || !gcal.status.sync_calendar) return;
  clearTimeout(pushTimer);
  pushTimer = setTimeout(() => {
    if (gcal.syncing) pushAgain = true;
    else void syncGcal();
  }, PUSH_DELAY_MS);
}
eventWriteHooks.push(schedulePush);

/** Stores a fresh status, reloads the calendar views and re-arms the timer. */
export function setGcalStatus(status: GcalStatus) {
  gcal.status = status;
  data.version++;
  schedule();
}

function schedule() {
  clearInterval(timer);
  const s = gcal.status;
  if (!s?.connected) return;
  timer = setInterval(syncGcal, Math.max(MIN_INTERVAL_MIN, s.interval_min) * 60_000);
}

export async function syncGcal() {
  if (gcal.syncing || !gcal.status?.connected) return;
  gcal.syncing = true;
  try {
    setGcalStatus(await api.gcalSync());
  } catch {
    // Errors are recorded in last_error by the backend; a rejected call means the app is closing.
  } finally {
    gcal.syncing = false;
    if (pushAgain) {
      pushAgain = false;
      schedulePush();
    }
  }
}

/** Called once at startup: syncs right away when connected, then on the chosen interval. */
export async function initGcal() {
  try {
    gcal.status = await api.gcalStatus();
  } catch {
    return;
  }
  if (gcal.status.connected) {
    schedule();
    void syncGcal();
  }
}
