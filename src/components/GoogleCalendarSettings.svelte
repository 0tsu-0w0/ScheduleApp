<script lang="ts">
  // Settings → Google Calendar: connect an account, pick calendars, sync (FR-12.1–12.6).
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { CalendarSync, ExternalLink, History, LoaderCircle, RefreshCw, RotateCcw, TriangleAlert, Unplug } from '@lucide/svelte';
  import Modal from './Modal.svelte';
  import { api, type GcalCalendar, type GcalReminder, type GsyncHistoryEntry, type GsyncPreview } from '../lib/api';
  import { hex } from '../lib/colors';
  import { dateOf, fmt, fmtTs, timeOf } from '../lib/dates';
  import { gcal, INTERVALS, setGcalStatus, syncGcal } from '../lib/gcal.svelte';
  import Select from './Select.svelte';
  import { t } from '../lib/i18n.svelte';
  import { reminderLabel } from '../lib/reminders';
  import { data, toast } from '../lib/state.svelte';

  const CONSOLE_URL = 'https://console.cloud.google.com/apis/credentials';

  let clientId = $state(gcal.status?.client_id ?? '');
  let clientSecret = $state('');
  let connecting = $state(false);
  let busy = $state(false);
  let confirmingDisconnect = $state(false);

  $effect(() => {
    if (!gcal.status) api.gcalStatus().then((s) => { gcal.status = s; clientId ||= s.client_id; }).catch(() => {});
  });

  const status = $derived(gcal.status);
  /** Has a configuration but no sign-in on this device, e.g. after restoring a backup. */
  const needsReconnect = $derived(!!status && !status.connected && status.calendars.length > 0);

  async function connect() {
    connecting = true;
    try {
      setGcalStatus(await api.gcalConnect(clientId, clientSecret));
      clientSecret = '';
      toast(t('gcal.connected'), 'success');
    } catch (e) {
      toast(String(e), 'error');
    } finally {
      connecting = false;
    }
  }

  async function act(p: () => Promise<typeof status>) {
    busy = true;
    try {
      const s = await p();
      if (s) setGcalStatus(s);
    } catch (e) {
      toast(String(e), 'error');
    } finally {
      busy = false;
    }
  }

  function update(calendars: GcalCalendar[], interval: number) {
    void act(() => api.gcalUpdate(calendars, interval));
  }

  /** Reminder choices for imported events (issue #1). */
  const REMINDERS: { value: GcalReminder; label: () => string }[] = [
    { value: 'off', label: () => t('remind.none') },
    { value: 'google', label: () => t('gcal.reminderGoogle') },
    ...[5, 10, 30, 60].map((n) => ({ value: { before: n }, label: () => reminderLabel(n) })),
  ];
  const sameReminder = (a: GcalReminder, b: GcalReminder) => JSON.stringify(a) === JSON.stringify(b);

  function setReminder(value: GcalReminder) {
    if (!status) return;
    void act(() => api.gcalUpdate(status.calendars, status.interval_min, value)).then(() => {
      // Google's own reminder times arrive with the next sync.
      if (value === 'google') void syncGcal();
    });
  }

  // ---- two-way sync (issue #4)
  const syncOptions = $derived([
    { value: '', label: t('gcal.twoWayOff') },
    ...(status?.calendars ?? []).filter((c) => c.writable).map((c) => ({ value: c.id, label: c.name, color: hex(c.color) })),
  ]);

  /** Asked before sync starts: what would be sent and received (issue #11). */
  let confirm = $state<{ id: string; name: string; preview: GsyncPreview | null } | null>(null);

  async function setSyncCalendar(id: string) {
    if (!id) {
      void act(() => api.gcalSetSyncCalendar(null));
      return;
    }
    confirm = { id, name: status?.calendars.find((c) => c.id === id)?.name ?? id, preview: null };
    try {
      const preview = await api.gcalSyncPreview(id);
      if (confirm?.id === id) confirm.preview = preview;
    } catch (e) {
      confirm = null;
      toast(String(e), 'error');
    }
  }

  function startSync() {
    const id = confirm?.id;
    confirm = null;
    if (!id) return;
    void act(() => api.gcalSetSyncCalendar(id)).then(() => {
      if (gcal.status?.sync_calendar === id) toast(t('gcal.twoWayOn'), 'success');
    });
  }

  // ---- history of replaced versions
  let history = $state<GsyncHistoryEntry[] | null>(null);

  async function openHistory() {
    history = await api.gcalSyncHistory().catch((e) => {
      toast(String(e), 'error');
      return null;
    });
  }

  async function restore(entry: GsyncHistoryEntry) {
    try {
      await api.gcalRestoreHistory(entry.id);
      data.version++;
      toast(t('gcal.restored', { name: entry.title }), 'success');
      await openHistory();
    } catch (e) {
      toast(String(e), 'error');
    }
  }

  const when = (e: GsyncHistoryEntry) =>
    `${fmt(dateOf(e.start), { month: 'short', day: 'numeric', weekday: 'short' })}${e.all_day ? '' : ' ' + timeOf(e.start)}`;

  async function allowWrite() {
    if (!status) return;
    connecting = true;
    try {
      setGcalStatus(await api.gcalConnect(status.client_id, '', true));
    } catch (e) {
      toast(String(e), 'error');
    } finally {
      connecting = false;
    }
  }

  function toggle(c: GcalCalendar, field: 'selected' | 'visible', on: boolean) {
    if (!status) return;
    const calendars = status.calendars.map((x) => (x.id === c.id ? { ...x, [field]: on } : x));
    update(calendars, status.interval_min);
    // Newly imported calendars need a fetch.
    if (field === 'selected' && on) setTimeout(syncGcal, 0);
  }
</script>

<section class="card">
  <div class="section-head">
    <div class="icon"><CalendarSync size={18} /></div>
    <div>
      <h2>{t('gcal.title')}</h2>
      <p class="muted small">{t('gcal.body')}</p>
    </div>
  </div>

  {#if status?.connected}
    <div class="row account">
      <span class="grow">{t('gcal.account', { name: status.account ?? '—' })}</span>
      {#if confirmingDisconnect}
        <span class="muted small">{t('gcal.disconnectConfirm')}</span>
        <button class="btn small danger" onclick={() => { confirmingDisconnect = false; void act(api.gcalDisconnect); }}>{t('gcal.disconnect')}</button>
        <button class="btn small ghost" onclick={() => (confirmingDisconnect = false)}>{t('common.cancel')}</button>
      {:else}
        <button class="btn small" onclick={() => (confirmingDisconnect = true)} disabled={busy}><Unplug size={15} /> {t('gcal.disconnect')}</button>
      {/if}
    </div>

    <div class="row sync">
      <span class="grow small" class:muted={!status.last_error}>
        {#if status.last_error}
          <span class="error"><TriangleAlert size={14} /> {t('gcal.failed', { error: status.last_error })}</span><br />
        {/if}
        {status.last_sync ? t('gcal.lastSync', { time: fmtTs(status.last_sync, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) }) : t('gcal.never')}
      </span>
      <button class="btn small" onclick={syncGcal} disabled={gcal.syncing}>
        {#if gcal.syncing}<span class="spin"><LoaderCircle size={15} /></span> {t('gcal.syncing')}{:else}<RefreshCw size={15} /> {t('gcal.sync')}{/if}
      </button>
    </div>

    <div class="row">
      <span class="grow strong">{t('gcal.interval')}</span>
      <div class="segmented">
        {#each INTERVALS as n (n)}
          <button class:active={status.interval_min === n} onclick={() => update(status.calendars, n)}>
            {n < 60 ? t('gcal.minutes', { n }) : t('gcal.hours', { n: n / 60 })}
          </button>
        {/each}
      </div>
    </div>

    <div class="row">
      <span class="grow">
        <span class="strong">{t('gcal.reminder')}</span><br />
        <span class="small muted">{t('gcal.reminderBody')}</span>
      </span>
      <div class="segmented wrap">
        {#each REMINDERS as r (JSON.stringify(r.value))}
          <button class:active={sameReminder(status.reminder ?? 'off', r.value)} onclick={() => setReminder(r.value)} disabled={busy}>{r.label()}</button>
        {/each}
      </div>
    </div>

    <div class="row two-way">
      <span class="grow">
        <span class="strong">{t('gcal.twoWay')}</span><br />
        <span class="small muted">{t('gcal.twoWayBody')}</span>
        {#if status.sync_calendar && status.sync_unsupported.length}
          <br /><span class="small muted">{t('gcal.twoWayUnsupported', { n: status.sync_unsupported.length })}</span>
        {/if}
        {#if status.sync_calendar}
          <br /><button class="link small" onclick={openHistory}><History size={13} /> {t('gcal.history')}</button>
        {/if}
      </span>
      {#if status.can_write}
        <div class="sync-select">
          <Select value={status.sync_calendar ?? ''} options={syncOptions} onchange={setSyncCalendar} label={t('gcal.twoWay')} />
        </div>
      {:else}
        <button class="btn small" onclick={allowWrite} disabled={connecting}>
          {#if connecting}<span class="spin"><LoaderCircle size={15} /></span>{/if} {t('gcal.allowWrite')}
        </button>
      {/if}
    </div>

    <div class="cal-head">
      <span class="strong grow">{t('gcal.calendars')}</span>
      <span class="col-label small muted">{t('gcal.import')}</span>
      <span class="col-label small muted">{t('gcal.show')}</span>
    </div>
    {#each status.calendars as c (c.id)}
      <div class="cal">
        <span class="swatch" style:--c={hex(c.color)}></span>
        <span class="grow truncate">{c.name}{#if status.sync_calendar === c.id}<span class="badge-two-way small">{t('gcal.twoWayBadge')}</span>{/if}</span>
        <span class="col-label"><input type="checkbox" class="switch" checked={c.selected} disabled={busy || status.sync_calendar === c.id} onchange={(e) => toggle(c, 'selected', e.currentTarget.checked)} /></span>
        <span class="col-label"><input type="checkbox" class="switch" checked={c.visible} disabled={busy || !c.selected} onchange={(e) => toggle(c, 'visible', e.currentTarget.checked)} /></span>
      </div>
    {/each}
    <button class="btn small ghost reload" onclick={() => act(api.gcalRefreshCalendars)} disabled={busy}><RefreshCw size={14} /> {t('gcal.reloadList')}</button>
  {:else}
    {#if needsReconnect}<p class="error small"><TriangleAlert size={14} /> {t('gcal.reconnect')}</p>{/if}
    <p class="small muted">
      {t('gcal.setupHint')}
      <button class="link" onclick={() => openUrl(CONSOLE_URL)}>Google Cloud Console <ExternalLink size={12} /></button>
    </p>
    <div class="fields">
      <label class="field">
        <span>{t('gcal.clientId')}</span>
        <input class="input" bind:value={clientId} placeholder="xxxx.apps.googleusercontent.com" autocomplete="off" spellcheck="false" />
      </label>
      <label class="field">
        <span>{t('gcal.clientSecret')}</span>
        <input class="input" type="password" bind:value={clientSecret} autocomplete="off" spellcheck="false" />
      </label>
    </div>
    <div class="row">
      <span class="grow small muted">{connecting ? t('gcal.connecting') : t('gcal.privacy')}</span>
      <button class="btn primary" onclick={connect} disabled={connecting || !clientId.trim() || !clientSecret.trim()}>
        {#if connecting}<span class="spin"><LoaderCircle size={16} /></span>{/if} {t('gcal.connect')}
      </button>
    </div>
  {/if}
</section>

{#if confirm}
  <Modal title={t('gcal.confirmTitle', { name: confirm.name })} onclose={() => (confirm = null)} width={460}>
    {#if confirm.preview}
      <ul class="preview-list">
        <li>{t('gcal.confirmSend', { n: confirm.preview.send })}</li>
        <li>{t('gcal.confirmReceive', { n: confirm.preview.receive })}</li>
        {#if confirm.preview.linked}<li>{t('gcal.confirmLinked', { n: confirm.preview.linked })}</li>{/if}
      </ul>
      <p class="small muted">{t('gcal.confirmNote')}</p>
    {:else}
      <p class="muted"><span class="spin"><LoaderCircle size={15} /></span> {t('gcal.confirmLoading')}</p>
    {/if}
    {#snippet footer()}
      <span class="spacer"></span>
      <button class="btn ghost" onclick={() => (confirm = null)}>{t('common.cancel')}</button>
      <button class="btn primary" onclick={startSync} disabled={!confirm?.preview}>{t('gcal.confirmStart')}</button>
    {/snippet}
  </Modal>
{/if}

{#if history}
  <Modal title={t('gcal.history')} onclose={() => (history = null)} width={520}>
    <p class="small muted">{t('gcal.historyBody')}</p>
    <div class="history">
      {#each history as h (h.id)}
        <div class="history-item">
          <span class="grow">
            <span class="strong">{h.title}</span> <span class="small muted">{when(h)}</span><br />
            <span class="small muted">{t(`gcal.reason.${h.reason}`)} · {fmtTs(Math.floor(h.at / 1000), { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}</span>
          </span>
          <button class="btn small" onclick={() => restore(h)}><RotateCcw size={14} /> {t('gcal.restore')}</button>
        </div>
      {:else}
        <p class="muted">{t('gcal.historyEmpty')}</p>
      {/each}
    </div>
  </Modal>
{/if}

<style>
  .section-head {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 18px;
  }
  .icon {
    width: 40px;
    height: 40px;
    border-radius: 13px;
    display: grid;
    place-items: center;
    flex: none;
    color: var(--primary);
    background: var(--primary-soft);
  }
  .row,
  .cal,
  .cal-head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 10px 14px;
  }
  .row {
    padding: 12px 0;
    border-top: 1px solid var(--border);
  }
  .cal-head {
    padding: 12px 0 6px;
    border-top: 1px solid var(--border);
  }
  .cal {
    padding: 6px 0;
    flex-wrap: nowrap;
  }
  .grow {
    flex: 1 1 200px;
    min-width: 0;
  }
  .cal .grow {
    flex: 1 1 auto;
  }
  .col-label {
    width: 56px;
    display: flex;
    justify-content: center;
    flex: none;
  }
  .swatch {
    width: 12px;
    height: 12px;
    border-radius: 4px;
    background: var(--c);
    flex: none;
  }
  .history {
    display: grid;
    gap: 2px;
    max-height: 420px;
    overflow: auto;
  }
  .history-item {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 0;
    border-top: 1px solid var(--border);
  }
  .history-item:first-child {
    border-top: 0;
  }
  .preview-list {
    margin: 0;
    padding-left: 20px;
    display: grid;
    gap: 6px;
  }
  .sync-select {
    min-width: 220px;
  }
  .badge-two-way {
    margin-left: 6px;
    padding: 1px 8px;
    border-radius: 999px;
    color: var(--primary);
    background: var(--primary-soft);
    font-weight: 650;
  }
  .segmented.wrap {
    flex-wrap: wrap;
  }
  .reload {
    margin-top: 6px;
  }
  .fields {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 12px;
    margin: 12px 0;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .error {
    color: var(--danger);
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .link {
    color: var(--primary);
    display: inline-flex;
    align-items: center;
    gap: 3px;
    padding: 0;
    background: none;
    border: 0;
    cursor: pointer;
    font: inherit;
  }
  .spin {
    display: inline-flex;
    animation: spin 1s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
