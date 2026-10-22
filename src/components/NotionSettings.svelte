<script lang="ts">
  // Settings → Notion (issue #5): export memos, books and TRPG records to the user's own
  // Notion databases, with a field-to-property mapping per kind. One way only.
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { BookOpenText, ExternalLink, LoaderCircle, RefreshCw, TriangleAlert, Unplug } from '@lucide/svelte';
  import Select from './Select.svelte';
  import { api, type NotionDatabase, type NotionKind, type NotionStatus, type NotionTarget } from '../lib/api';
  import { fmtTs } from '../lib/dates';
  import { t, type Key } from '../lib/i18n.svelte';
  import { toast } from '../lib/state.svelte';

  const INTEGRATIONS_URL = 'https://www.notion.so/profile/integrations';
  const KINDS: NotionKind[] = ['memo', 'book', 'trpg'];
  /** Which Notion property types each kind of Nora value can go into (matches notion.rs). */
  const ACCEPTS: Record<string, string[]> = {
    text: ['title', 'rich_text', 'select', 'multi_select', 'status'],
    list: ['multi_select', 'rich_text', 'select'],
    number: ['number', 'rich_text'],
    date: ['date', 'rich_text'],
    url: ['url', 'rich_text'],
  };

  let status = $state<NotionStatus | null>(null);
  let token = $state('');
  let busy = $state(false);
  let exporting = $state<NotionKind | 'all' | null>(null);

  /** The kind whose mapping is being edited, with the database read for it. */
  let editing = $state<{ kind: NotionKind; link: string; db: NotionDatabase | null; mapping: Record<string, string>; body: boolean } | null>(null);

  $effect(() => {
    api.notionStatus().then((s) => (status = s)).catch(() => {});
  });

  const fieldLabel = (id: string) => t(`notion.f.${id}` as Key);
  const fieldsOf = (kind: NotionKind) => status?.fields.find((f) => f.kind === kind)?.fields ?? [];

  async function run<T>(p: () => Promise<T>): Promise<T | undefined> {
    busy = true;
    try {
      return await p();
    } catch (e) {
      toast(String(e), 'error');
      return undefined;
    } finally {
      busy = false;
    }
  }

  async function connect() {
    const s = await run(() => api.notionConnect(token));
    if (s) {
      status = s;
      token = '';
      toast(t('notion.connected'), 'success');
    }
  }

  function startEdit(kind: NotionKind) {
    const target = status?.config.targets[kind];
    editing = {
      kind,
      link: target?.database_id ?? '',
      db: target ? { id: target.database_id, title: target.database_title, properties: target.types } : null,
      mapping: { ...(target?.mapping ?? {}) },
      body: target?.body ?? true,
    };
  }

  /** Suggests a property for each field: the title property for the title, otherwise a same-named one. */
  function guessMapping(kind: NotionKind, db: NotionDatabase): Record<string, string> {
    const out: Record<string, string> = {};
    const props = Object.entries(db.properties);
    for (const f of fieldsOf(kind)) {
      const fits = props.filter(([, ty]) => ACCEPTS[f.value].includes(ty));
      const hit =
        f.id === 'title'
          ? fits.find(([, ty]) => ty === 'title')
          : fits.find(([name]) => [f.id, fieldLabel(f.id)].some((n) => n.toLowerCase() === name.toLowerCase()));
      if (hit) out[f.id] = hit[0];
    }
    return out;
  }

  async function loadDatabase() {
    if (!editing) return;
    const db = await run(() => api.notionDatabase(editing!.link));
    if (db && editing) {
      editing.db = db;
      editing.mapping = guessMapping(editing.kind, db);
    }
  }

  function options(kind: NotionKind, fieldId: string) {
    const value = fieldsOf(kind).find((f) => f.id === fieldId)?.value ?? 'text';
    const props = Object.entries(editing?.db?.properties ?? {}).filter(([, ty]) => ACCEPTS[value].includes(ty));
    return [{ value: '', label: t('notion.none') }, ...props.map(([name, ty]) => ({ value: name, label: `${name} (${t(`notion.t.${ty}` as Key)})` }))];
  }

  async function saveTarget() {
    if (!editing?.db) return;
    const mapping = Object.fromEntries(Object.entries(editing.mapping).filter(([, p]) => p));
    const target: NotionTarget = {
      database_id: editing.db.id,
      database_title: editing.db.title,
      mapping,
      types: editing.db.properties,
      body: editing.body,
    };
    const s = await run(() => api.notionSetTarget(editing!.kind, target));
    if (s) {
      status = s;
      editing = null;
    }
  }

  async function removeTarget(kind: NotionKind) {
    const s = await run(() => api.notionSetTarget(kind, null));
    if (s) status = s;
  }

  async function exportNow(kind: NotionKind | null) {
    exporting = kind ?? 'all';
    try {
      status = await api.notionExport(kind);
      const r = status.last_report;
      if (status.last_error) toast(status.last_error, 'error');
      else if (r) toast(t('notion.done', { created: r.created, updated: r.updated, archived: r.archived }), 'success');
    } catch (e) {
      toast(String(e), 'error');
    } finally {
      exporting = null;
    }
  }

  const hasTitle = $derived(!!editing?.db && Object.values(editing.db.properties).includes('title'));
</script>

{#if status}
  <section class="card">
    <div class="section-head">
      <div class="icon"><BookOpenText size={18} /></div>
      <div>
        <h2>{t('notion.title')}</h2>
        <p class="muted small">{t('notion.body')}</p>
      </div>
    </div>

    {#if !status.connected}
      <ol class="steps small muted">
        <li>
          {t('notion.step1')}
          <button class="link" onclick={() => openUrl(INTEGRATIONS_URL)}>Notion <ExternalLink size={12} /></button>
        </li>
        <li>{t('notion.step2')}</li>
        <li>{t('notion.step3')}</li>
      </ol>
      <div class="row">
        <input class="input grow" type="password" bind:value={token} placeholder="ntn_…" autocomplete="off" spellcheck="false" aria-label={t('notion.token')} />
        <button class="btn primary" onclick={connect} disabled={busy || !token.trim()}>
          {#if busy}<span class="spin"><LoaderCircle size={16} /></span>{/if} {t('notion.connect')}
        </button>
      </div>
    {:else}
      <div class="row">
        <span class="grow">{t('notion.account', { name: status.config.workspace ?? 'Notion' })}</span>
        <button class="btn small" onclick={() => run(api.notionDisconnect).then((s) => s && (status = s))}><Unplug size={15} /> {t('notion.disconnect')}</button>
      </div>

      <label class="row clickable">
        <span class="grow">
          <span class="strong">{t('notion.auto')}</span><br />
          <span class="small muted">{t('notion.autoBody')}</span>
        </span>
        <input type="checkbox" class="switch" checked={status.config.auto} onchange={(e) => run(() => api.notionSetAuto(e.currentTarget.checked)).then((s) => s && (status = s))} />
      </label>

      {#each KINDS as kind (kind)}
        {@const target = status.config.targets[kind]}
        <div class="kind">
          <div class="kind-head">
            <span class="strong grow">{t(`notion.kind.${kind}` as Key)}</span>
            {#if target && editing?.kind !== kind}
              <button class="btn small" onclick={() => exportNow(kind)} disabled={!!exporting}>
                {#if exporting === kind}<span class="spin"><LoaderCircle size={14} /></span>{/if} {t('notion.export')}
              </button>
              <button class="btn small ghost" onclick={() => startEdit(kind)}>{t('notion.change')}</button>
              <button class="btn small ghost" onclick={() => removeTarget(kind)}>{t('notion.remove')}</button>
            {:else if !target && editing?.kind !== kind}
              <button class="btn small" onclick={() => startEdit(kind)}>{t('notion.setUp')}</button>
            {/if}
          </div>
          {#if target && editing?.kind !== kind}
            <p class="small muted">
              {t('notion.to', { name: target.database_title || target.database_id })} ·
              {Object.entries(target.mapping).map(([f, p]) => `${fieldLabel(f)} → ${p}`).join(', ') || t('notion.onlyTitle')}
              {#if target.body} · {t(`notion.body.${kind}` as Key)}{/if}
            </p>
          {/if}

          {#if editing?.kind === kind}
            <div class="edit">
              <div class="row tight">
                <input class="input grow" bind:value={editing.link} placeholder={t('notion.dbLink')} spellcheck="false" />
                <button class="btn small" onclick={loadDatabase} disabled={busy || !editing.link.trim()}>
                  {#if busy}<span class="spin"><LoaderCircle size={14} /></span>{/if} {t('notion.load')}
                </button>
              </div>
              {#if editing.db}
                <p class="small muted">{t('notion.to', { name: editing.db.title || editing.db.id })}</p>
                {#if !hasTitle}<p class="error small"><TriangleAlert size={14} /> {t('notion.noTitle')}</p>{/if}
                <div class="mapping">
                  {#each fieldsOf(kind) as f (f.id)}
                    <span class="small">{fieldLabel(f.id)}</span>
                    <Select value={editing.mapping[f.id] ?? ''} options={options(kind, f.id)} onchange={(v) => editing && (editing.mapping[f.id] = v)} label={fieldLabel(f.id)} />
                  {/each}
                </div>
                <label class="row tight clickable">
                  <input type="checkbox" bind:checked={editing.body} />
                  <span class="small">{t('notion.writeBody', { what: t(`notion.body.${kind}` as Key) })}</span>
                </label>
              {/if}
              <div class="row tight end">
                <button class="btn small ghost" onclick={() => (editing = null)}>{t('common.cancel')}</button>
                <button class="btn small primary" onclick={saveTarget} disabled={busy || !editing.db || !hasTitle}>{t('common.save')}</button>
              </div>
            </div>
          {/if}
        </div>
      {/each}

      <div class="row">
        <span class="grow small" class:muted={!status.last_error}>
          {#if status.last_error}<span class="error"><TriangleAlert size={14} /> {status.last_error}</span><br />{/if}
          {#if status.last_run}
            {t('notion.last', { time: fmtTs(status.last_run, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) })}
            {#if status.last_report}
              · {t('notion.report', { created: status.last_report.created, updated: status.last_report.updated, unchanged: status.last_report.unchanged, archived: status.last_report.archived })}
              {#if status.last_report.skipped_elements}<br /><span class="warn">{t('notion.skipped', { n: status.last_report.skipped_elements })}</span>{/if}
            {/if}
          {:else}
            {t('notion.never')}
          {/if}
        </span>
        <button class="btn small" onclick={() => exportNow(null)} disabled={!!exporting || !Object.keys(status.config.targets).length}>
          {#if exporting === 'all'}<span class="spin"><LoaderCircle size={15} /></span>{:else}<RefreshCw size={15} />{/if} {t('notion.exportAll')}
        </button>
      </div>
    {/if}
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
  .row.tight {
    padding: 6px 0;
    border-top: 0;
  }
  .row.end {
    justify-content: flex-end;
  }
  .clickable {
    cursor: pointer;
  }
  .grow {
    flex: 1 1 220px;
    min-width: 0;
  }
  .steps {
    margin: 0 0 8px;
    padding-left: 20px;
    display: grid;
    gap: 4px;
  }
  .kind {
    padding: 12px 0;
    border-top: 1px solid var(--border);
  }
  .kind-head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
  }
  .kind p {
    margin: 6px 0 0;
    overflow-wrap: anywhere;
  }
  .edit {
    margin-top: 8px;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }
  .mapping {
    display: grid;
    grid-template-columns: minmax(90px, 160px) minmax(0, 1fr);
    align-items: center;
    gap: 8px 12px;
    margin: 10px 0;
  }
  .error {
    color: var(--danger);
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .warn {
    color: var(--danger);
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
