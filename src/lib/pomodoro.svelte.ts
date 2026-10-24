// Pomodoro timer. Lives outside the page so it keeps running while you browse
// other pages. Uses wall-clock deadlines, so it stays accurate in the background.
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification';
import { api } from './api';
import { data, toast } from './state.svelte';
import { t } from './i18n.svelte';

export type Phase = 'focus' | 'short' | 'long';

export const PHASES: Record<Phase, { name: () => string; color: string }> = {
  focus: { name: () => t('pomo.focus'), color: 'var(--focus)' },
  short: { name: () => t('pomo.short'), color: 'var(--break)' },
  long: { name: () => t('pomo.long'), color: 'var(--long-break)' },
};

const SETTINGS = ['focusMin', 'shortMin', 'longMin', 'longEvery', 'autoStart'] as const;

/** Focus shorter than this is not saved when stopped early (an accidental start). */
const MIN_SAVED_SECS = 60;
/** How often a running focus session is saved, so quitting the app loses at most this much. */
const SAVE_EVERY_MS = 60_000;

/** The focus session being recorded. It is saved as it goes, so stopping early never loses it. */
interface FocusRecord {
  /** Database row once saved. */
  id: number | null;
  startedTs: number;
  /** Time actually focused, without pauses. */
  focusedMs: number;
}

class Pomodoro {
  focusMin = $state(25);
  shortMin = $state(5);
  longMin = $state(15);
  longEvery = $state(4);
  autoStart = $state(false);

  phase = $state<Phase>('focus');
  label = $state('');
  completed = $state(0);
  /** Deadline (ms) while running. */
  endsAt = $state<number | null>(null);
  /** Remaining time (ms) while paused or not started. */
  pausedRemaining = $state(25 * 60_000);
  /** Whether the current phase has been started (it may be paused now). */
  started = $state(false);
  now = $state(Date.now());
  private record: FocusRecord | null = null;
  /** When the timer was last started or its time last added to `record` (focus only). */
  private runStartedAt: number | null = null;
  private lastSaved = 0;
  /** Saves run one after another, so a session is never inserted twice. */
  private saving: Promise<void> = Promise.resolve();
  /** Runs only while the timer is running; nothing ticks while it is paused or stopped. */
  private ticker: ReturnType<typeof setInterval> | undefined;

  get running() {
    return this.endsAt !== null;
  }
  get phaseMs() {
    const min = this.phase === 'focus' ? this.focusMin : this.phase === 'short' ? this.shortMin : this.longMin;
    return min * 60_000;
  }
  get remaining() {
    return this.endsAt !== null ? Math.max(0, this.endsAt - this.now) : this.pausedRemaining;
  }
  get progress() {
    return this.phaseMs > 0 ? this.remaining / this.phaseMs : 0;
  }
  get text() {
    const secs = Math.ceil(this.remaining / 1000);
    return `${String(Math.floor(secs / 60)).padStart(2, '0')}:${String(secs % 60).padStart(2, '0')}`;
  }

  async load() {
    for (const key of SETTINGS) {
      const v = await api.getSetting(`pomodoro.${key}`).catch(() => null);
      if (v === null) continue;
      if (key === 'autoStart') this.autoStart = v === 'true';
      else if (Number(v) > 0) this[key] = Number(v);
    }
    if (!this.started) this.pausedRemaining = this.phaseMs;
  }

  async saveSettings() {
    for (const key of SETTINGS) await api.setSetting(`pomodoro.${key}`, String(this[key])).catch(() => {});
    if (!this.started) this.pausedRemaining = this.phaseMs;
  }

  start() {
    if (this.phase === 'focus') {
      this.record ??= { id: null, startedTs: Math.floor(Date.now() / 1000), focusedMs: 0 };
      this.runStartedAt = this.lastSaved = Date.now();
    }
    this.started = true;
    this.now = Date.now();
    this.endsAt = this.now + this.pausedRemaining;
    this.ticker ??= setInterval(() => this.tick(), 250);
    // Ask once, early, so the end-of-session notification can be shown.
    isPermissionGranted()
      .then((ok) => {
        if (!ok) requestPermission();
      })
      .catch(() => {});
  }

  pause() {
    this.pausedRemaining = this.remaining;
    this.endsAt = null;
    if (this.phase === 'focus') {
      this.bank(Date.now());
      this.runStartedAt = null;
      // Saved now, in case the app is closed before the session is resumed.
      this.saveFocus(false);
    }
  }

  toggle() {
    if (this.running) this.pause();
    else this.start();
  }

  setPhase(phase: Phase) {
    // Leaving a focus session early (skip, switching phase, reset) keeps the time focused so far.
    if (this.record) {
      if (this.endsAt !== null) this.bank(Date.now());
      this.saveFocus(false);
      this.record = null;
    }
    this.runStartedAt = null;
    this.phase = phase;
    this.endsAt = null;
    this.started = false;
    this.pausedRemaining = this.phaseMs;
  }

  reset() {
    this.setPhase(this.phase);
  }

  /** Moves to the next phase. `finished` = the timer ran out (at `end`); otherwise it was skipped. */
  advance(finished: boolean, end = Date.now()) {
    let next: Phase = 'focus';
    if (this.phase === 'focus') {
      if (finished && this.record) {
        this.saveFocus(true, end);
        this.record = null;
        this.completed++;
      }
      next = this.completed > 0 && this.completed % Math.max(1, this.longEvery) === 0 ? 'long' : 'short';
    }
    this.setPhase(next);
    if (finished && this.autoStart) this.start();
  }

  /** Adds the time since the last start (or last call) to the focus record. */
  private bank(until: number) {
    if (!this.record || this.runStartedAt === null) return;
    this.record.focusedMs += Math.max(0, until - this.runStartedAt);
    this.runStartedAt = until;
  }

  /**
   * Saves the current focus session (creating it the first time, updating it after that).
   * `refresh` tells open pages to reload; it is off for the periodic save while running.
   */
  private saveFocus(completed: boolean, endMs = Date.now(), refresh = true) {
    const r = this.record;
    if (!r) return;
    const focusedSecs = Math.round(r.focusedMs / 1000);
    if (!completed && r.id === null && focusedSecs < MIN_SAVED_SECS) return;
    const endedTs = Math.max(r.startedTs, Math.floor(endMs / 1000));
    const label = this.label;
    this.saving = this.saving.then(async () => {
      try {
        r.id = await api.savePomodoroSession(r.id, r.startedTs, endedTs, focusedSecs, completed, label);
        if (refresh) data.version++;
      } catch (e) {
        toast(String(e), 'error');
      }
    });
  }

  private tick() {
    this.now = Date.now();
    if (this.endsAt === null) {
      clearInterval(this.ticker);
      this.ticker = undefined;
      return;
    }
    if (this.now < this.endsAt) {
      if (this.phase === 'focus' && this.now - this.lastSaved >= SAVE_EVERY_MS) {
        this.lastSaved = this.now;
        this.bank(this.now);
        this.saveFocus(false, this.now, false);
      }
      return;
    }
    const end = this.endsAt;
    const wasFocus = this.phase === 'focus';
    if (wasFocus) {
      this.bank(end);
      this.runStartedAt = null;
    }
    this.endsAt = null;
    // Stop ticking unless auto-start begins the next phase right away.
    clearInterval(this.ticker);
    this.ticker = undefined;
    this.advance(true, end);
    const body = wasFocus ? t('pomo.doneFocus') : t('pomo.doneBreak');
    toast(body, 'success');
    sendNotification({ title: 'ScheduleApp', body });
  }
}

export const pomodoro = new Pomodoro();
