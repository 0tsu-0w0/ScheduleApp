<script lang="ts">
  // Settings → Google Calendar: connect an account, pick calendars, sync (FR-12.1–12.6).
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { CalendarSync, ExternalLink, LoaderCircle, RefreshCw, TriangleAlert, Unplug } from '@lucide/svelte';
  import { api, type GcalCalendar } from '../lib/api';
  import { hex } from '../lib/colors';
  import { fmtTs } from '../lib/dates';
  import { gcal, INTERVALS, setGcalStatus, syncGcal } from '../lib/gcal.svelte';
  import { t } from '../lib/i18n.svelte';
  import { toast } from '../lib/state.svelte';

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

    <div class="cal-head">
      <span class="strong grow">{t('gcal.calendars')}</span>
      <span class="col-label small muted">{t('gcal.import')}</span>
      <span class="col-label small muted">{t('gcal.show')}</span>
    </div>
    {#each status.calendars as c (c.id)}
      <div class="cal">
        <span class="swatch" style:--c={hex(c.color)}></span>
        <span class="grow truncate">{c.name}</span>
        <span class="col-label"><input type="checkbox" class="switch" checked={c.selected} disabled={busy} onchange={(e) => toggle(c, 'selected', e.currentTarget.checked)} /></span>
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
