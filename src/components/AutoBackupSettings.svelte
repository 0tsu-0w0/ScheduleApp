<script lang="ts">
  // Settings → Automatic backups (issue #2): folder, interval, how many to keep.
  import { open } from '@tauri-apps/plugin-dialog';
  import { ArchiveRestore, FolderOpen, LoaderCircle, TriangleAlert } from '@lucide/svelte';
  import { api, type AutoBackupConfig, type AutoBackupStatus } from '../lib/api';
  import { backupError } from '../lib/backup';
  import { fmtTs } from '../lib/dates';
  import { t } from '../lib/i18n.svelte';
  import { toast } from '../lib/state.svelte';

  const KEEP = [3, 7, 14, 30];

  let status = $state<AutoBackupStatus | null>(null);
  let running = $state(false);

  $effect(() => {
    api.autoBackupStatus().then((s) => (status = s)).catch(() => {});
  });

  const config = $derived(status?.config);

  async function update(patch: Partial<AutoBackupConfig>) {
    if (!config) return;
    try {
      status = await api.autoBackupUpdate({ ...config, ...patch });
    } catch (e) {
      toast(String(e), 'error');
    }
  }

  async function pickFolder() {
    const dir = await open({ directory: true, multiple: false, defaultPath: config?.dir || undefined }).catch(() => null);
    if (typeof dir === 'string') await update({ dir });
  }

  async function runNow() {
    running = true;
    try {
      status = await api.autoBackupRunNow();
      if (status.last_error) toast(backupError(status.last_error), 'error');
      else toast(t('auto.done', { name: status.last_file ?? '' }), 'success');
    } catch (e) {
      toast(backupError(e), 'error');
    } finally {
      running = false;
    }
  }

  const when = (ts: number) => fmtTs(ts, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
</script>

{#if config && status}
  <section class="card">
    <div class="section-head">
      <div class="icon"><ArchiveRestore size={18} /></div>
      <div>
        <h2>{t('auto.title')}</h2>
        <p class="muted small">{t('auto.body')}</p>
      </div>
      <span class="spacer"></span>
      <input type="checkbox" class="switch" checked={config.enabled} onchange={(e) => update({ enabled: e.currentTarget.checked })} aria-label={t('auto.title')} />
    </div>

    <div class="row">
      <span class="grow">
        <span class="strong">{t('auto.folder')}</span><br />
        <span class="small" class:muted={!!config.dir} class:error={!config.dir && config.enabled}>{config.dir || t('auto.noFolder')}</span>
      </span>
      <button class="btn small" onclick={pickFolder}><FolderOpen size={15} /> {t('auto.choose')}</button>
    </div>

    <div class="row">
      <span class="grow strong">{t('auto.interval')}</span>
      <div class="segmented">
        <button class:active={config.interval_days === 1} onclick={() => update({ interval_days: 1 })}>{t('auto.daily')}</button>
        <button class:active={config.interval_days === 7} onclick={() => update({ interval_days: 7 })}>{t('auto.weekly')}</button>
      </div>
    </div>

    <div class="row">
      <span class="grow">
        <span class="strong">{t('auto.keep')}</span><br />
        <span class="small muted">{t('auto.keepBody')}</span>
      </span>
      <div class="segmented">
        {#each KEEP as n (n)}
          <button class:active={config.keep === n} onclick={() => update({ keep: n })}>{t('auto.copies', { n })}</button>
        {/each}
      </div>
    </div>

    <div class="row">
      <span class="grow small" class:muted={!status.last_error}>
        {#if status.last_error}
          <span class="error"><TriangleAlert size={14} /> {t('auto.failed', { error: backupError(status.last_error) })}</span><br />
        {/if}
        {status.last_run ? t('auto.last', { time: when(status.last_run) }) : t('auto.never')}
      </span>
      <button class="btn small" onclick={runNow} disabled={running || !config.dir}>
        {#if running}<span class="spin"><LoaderCircle size={15} /></span>{/if} {t('auto.runNow')}
      </button>
    </div>
  </section>
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
  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 10px 14px;
    padding: 12px 0;
    border-top: 1px solid var(--border);
  }
  .grow {
    flex: 1 1 220px;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .error {
    color: var(--danger);
    display: inline-flex;
    align-items: center;
    gap: 6px;
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
