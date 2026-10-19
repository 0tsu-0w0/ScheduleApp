<script lang="ts">
  // Read-only details of an event imported from Google Calendar.
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { CalendarDays, ExternalLink, MapPin } from '@lucide/svelte';
  import Modal from './Modal.svelte';
  import type { CalEvent } from '../lib/api';
  import { hex } from '../lib/colors';
  import { dateOf, eventSpan, fmt, fmtRange, timeOf } from '../lib/dates';
  import { t } from '../lib/i18n.svelte';

  let { event, onclose }: { event: CalEvent; onclose: () => void } = $props();

  const when = $derived.by(() => {
    const [a, b] = eventSpan(event);
    const day = (d: string) => fmt(d, { month: 'short', day: 'numeric', weekday: 'short' });
    if (event.all_day) return a === b ? `${day(a)} · ${t('common.allDay')}` : fmtRange(a, b, { month: 'short', day: 'numeric' });
    if (a === b) return `${day(a)} ${timeOf(event.start)} – ${timeOf(event.end)}`;
    return `${day(a)} ${timeOf(event.start)} – ${day(dateOf(event.end))} ${timeOf(event.end)}`;
  });
</script>

<Modal title={event.title} {onclose} width={440}>
  <div class="details" style:--c={hex(event.color)}>
    <div class="row"><span class="swatch"></span> {when}</div>
    {#if event.location}<div class="row"><MapPin size={15} /> <span>{event.location}</span></div>{/if}
    <div class="row muted small"><CalendarDays size={15} /> {t('gcal.readOnly', { name: event.external?.calendar_name ?? '' })}</div>
    {#if event.memo.trim()}<p class="memo">{event.memo}</p>{/if}
  </div>
  {#snippet footer()}
    <span class="spacer"></span>
    <button class="btn ghost" onclick={onclose}>{t('common.close')}</button>
    {#if event.external?.url}
      <button class="btn primary" onclick={() => openUrl(event.external!.url)}><ExternalLink size={16} /> {t('gcal.openInGoogle')}</button>
    {/if}
  {/snippet}
</Modal>

<style>
  .details {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .swatch {
    width: 12px;
    height: 12px;
    border-radius: 4px;
    background: var(--c);
    flex: none;
  }
  .memo {
    margin: 4px 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 260px;
    overflow: auto;
  }
</style>
