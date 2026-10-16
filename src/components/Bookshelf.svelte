<script lang="ts" generics="T extends { id: number; title: string; image: string | null }">
  // Books shown either on a shelf (covers standing on planks) or as a plain list.
  // `meta` renders the line under each title (system, author, stars…) in both views.
  // With `onreorder`, books can be dragged into a new order in either view.
  import type { Snippet } from 'svelte';
  import BookCover from './BookCover.svelte';

  let {
    items,
    view = 'shelf',
    author,
    onopen,
    onmenu,
    meta,
    trailing,
    onreorder,
  }: {
    items: T[];
    view?: 'shelf' | 'list';
    author: (b: T) => string;
    onopen: (b: T) => void;
    onmenu?: (ev: MouseEvent, b: T) => void;
    meta: Snippet<[T]>;
    /** Extra content at the end of a list row (e.g. progress). */
    trailing?: Snippet<[T]>;
    /** Makes books draggable; called with the ids first to last after a drag changed the order. */
    onreorder?: (ids: number[]) => void;
  } = $props();

  // While dragging, the books are reordered live so you can see where the dragged one will land.
  let order = $state<T[] | null>(null);
  let dragId = $state<number | null>(null);
  let changed = false;
  const shown = $derived(order ?? items);

  function dragStart(e: DragEvent, b: T) {
    if (!onreorder) return;
    e.dataTransfer?.setData('application/x-nora-book', String(b.id));
    if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
    dragId = b.id;
    order = [...items];
    changed = false;
  }

  function dragOver(e: DragEvent, target: T) {
    if (dragId === null || !order) return;
    e.preventDefault();
    if (target.id === dragId) return;
    const from = order.findIndex((b) => b.id === dragId);
    const to = order.findIndex((b) => b.id === target.id);
    if (from < 0 || to < 0) return;
    const next = [...order];
    next.splice(to, 0, ...next.splice(from, 1));
    order = next;
    changed = true;
  }

  function dragEnd() {
    if (order && changed) onreorder?.(order.map((b) => b.id));
    dragId = null;
    order = null;
  }

  /** Drag handlers for one book (nothing when the order is fixed). */
  const draggable = (b: T) =>
    onreorder
      ? {
          draggable: true,
          ondragstart: (e: DragEvent) => dragStart(e, b),
          ondragover: (e: DragEvent) => dragOver(e, b),
          ondrop: (e: DragEvent) => e.preventDefault(),
          ondragend: dragEnd,
        }
      : {};
</script>

{#if view === 'shelf'}
  <div class="shelf" class:reorderable={!!onreorder}>
    {#each shown as b (b.id)}
      <button class="book" class:dragging={dragId === b.id} onclick={() => onopen(b)} oncontextmenu={(ev) => onmenu?.(ev, b)} title={b.title} {...draggable(b)}>
        <BookCover title={b.title} author={author(b)} image={b.image} />
        <span class="caption">
          <span class="caption-title">{b.title}</span>
          <span class="caption-meta faint small">{@render meta(b)}</span>
        </span>
      </button>
    {/each}
  </div>
{:else}
  <div class="list" class:reorderable={!!onreorder}>
    {#each shown as b (b.id)}
      <button class="row" class:dragging={dragId === b.id} onclick={() => onopen(b)} oncontextmenu={(ev) => onmenu?.(ev, b)} {...draggable(b)}>
        <span class="thumb"><BookCover title={b.title} image={b.image} small /></span>
        <span class="row-text">
          <span class="row-title truncate">{b.title}</span>
          <span class="muted small truncate row-meta">{@render meta(b)}</span>
        </span>
        {#if trailing}{@render trailing(b)}{/if}
      </button>
    {/each}
  </div>
{/if}

<style>
  /* ---- shelf: each grid row is a shelf; books stand on the plank, titles below it */
  .shelf {
    --cover-w: 124px;
    --cover-h: 178px;
    --top: 22px;
    --plank: 12px;
    --row: calc(var(--top) + var(--cover-h) + var(--plank) + 52px);
    --plank-at: calc(var(--top) + var(--cover-h));
    display: grid;
    grid-template-columns: repeat(auto-fill, var(--cover-w));
    grid-auto-rows: var(--row);
    justify-content: space-evenly;
    column-gap: 24px;
    padding: 0 24px;
    background: repeating-linear-gradient(
      to bottom,
      transparent 0,
      transparent var(--plank-at),
      color-mix(in srgb, var(--muted) 30%, var(--surface-2)) var(--plank-at),
      color-mix(in srgb, var(--muted) 22%, var(--surface-2)) calc(var(--plank-at) + var(--plank) - 3px),
      color-mix(in srgb, var(--text) 18%, var(--surface)) calc(var(--plank-at) + var(--plank) - 3px),
      color-mix(in srgb, var(--text) 18%, var(--surface)) calc(var(--plank-at) + var(--plank)),
      transparent calc(var(--plank-at) + var(--plank)),
      transparent var(--row)
    );
  }
  .book {
    display: flex;
    flex-direction: column;
    width: var(--cover-w);
    padding: var(--top) 0 0;
    border: none;
    background: none;
    text-align: left;
    cursor: pointer;
  }
  .book:hover :global(.cover) {
    transform: translateY(-6px);
  }
  .caption {
    display: flex;
    flex-direction: column;
    gap: 1px;
    margin-top: calc(var(--plank) + 8px);
    min-width: 0;
  }
  .caption-title {
    font-size: 13px;
    font-weight: 650;
    display: -webkit-box;
    -webkit-line-clamp: 1;
    line-clamp: 1;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .caption-meta {
    display: flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* ---- list */
  .list {
    display: flex;
    flex-direction: column;
    padding: 8px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 8px 10px;
    border: none;
    border-radius: 12px;
    background: none;
    text-align: left;
    cursor: pointer;
  }
  .row:hover {
    background: var(--surface-2);
  }
  .thumb {
    --cover-w: 34px;
    --cover-h: 48px;
    display: grid;
  }
  .row-text {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
    flex: 1;
  }
  .row-title {
    font-weight: 650;
  }
  .row-meta {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .reorderable .book,
  .reorderable .row {
    cursor: grab;
  }
  .book.dragging,
  .row.dragging {
    opacity: 0.4;
  }

  @container main (max-width: 560px) {
    .shelf {
      --cover-w: 104px;
      --cover-h: 150px;
      column-gap: 16px;
      padding: 0 12px;
    }
  }
</style>
