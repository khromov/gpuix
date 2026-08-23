# @gpuix/svelte

Svelte's [custom renderer API](https://github.com/sveltejs/svelte/pull/18042) targeting Zed's GPUI —
the Svelte counterpart to `@gpuix/react`.

```svelte
<!-- Counter.svelte -->
<script>
	let count = $state(0);
</script>

<div style="padding: 32px; background-color: #1e1e2e; border-radius: 12px">
	<div style="font-size: 48px; color: #cdd6f4; cursor: pointer" onclick={() => count++}>
		{count}
	</div>
</div>
```

```js
import { render_hot } from '@gpuix/svelte';

render_hot(new URL('./Counter.svelte', import.meta.url), {
	title: 'GPUIX + Svelte',
	width: 820,
	height: 560
});
```

```
cd examples && bun run counter-svelte
```

## Setup

This package depends on an unreleased Svelte branch (`custom-condition`), checked out as a sibling of
this repo. `bun install` *copies* `file:` dependencies rather than symlinking them, so after every
install run:

```
./scripts/link-svelte.sh          # SVELTE_REPO=... to point elsewhere
```

The `--conditions custom-renderer` flag in the run script is **not optional**. Svelte's `exports` map
resolves to its *server* build by default outside a browser, and `mount()` doesn't exist there.
Without the condition you get a descriptive throw from `svelte/internal/flags/custom-renderer`.

## Hot reload

`render_hot` watches for `.svelte` changes and remounts. Use `render(Component, options)` instead if
you don't want a watcher.

Note that `bun --hot` cannot do this job here, for two independent reasons:

- `.svelte` files are plugin-loaded, so they never enter Bun's watch graph and editing one triggers
  nothing. (Bun also ignores mtime-only changes — a `touch` is not enough, the content must differ.)
- A `--hot` reload re-evaluates *Svelte's runtime* too, so the previous component belongs to a module
  instance the new one can't see and `unmount` reports it as never mounted.

Watching in-process avoids both: one Svelte runtime lives for the life of the process, the old tree
unmounts properly, and only the component module is re-instantiated. The loader propagates its
cache-busting `?v=` query to child components so a reload doesn't re-instantiate the root against
stale children.

Set `GPUIX_SCREENSHOT=/path/to.png` to have each mount write a PNG — a window can't be inspected
from a terminal, and Preview.app reloads on write.

## How it works

GPUI's tree is flat and id-based, and knows only `div`, `text` and its custom element types. Svelte's
tree is DOM-shaped and full of **anchor nodes** — comments and empty text nodes marking every
`{#if}`, `{#each}` and component boundary — plus fragments. None of those exist in GPUI.

So `src/renderer.js` keeps a **JS shadow tree** and projects it onto GPUI:

| Svelte node | GPUI |
|---|---|
| element | `createElement(id, tag)` |
| text with content | `createElement(id, 'text')` + `setText` |
| text that is empty or whitespace-only | *nothing* — materialized if it later gains content |
| comment | *nothing, ever* |
| fragment | splatted into the parent on insert |

Three properties make this tractable:

- **Virtual nodes are always leaves**, so "the next native node" is a flat scan of following
  siblings. A native node can never hide beneath a virtual one.
- **Native ids are allocated lazily**, when a node first becomes reachable from the root. Svelte
  renders offscreen constantly (the shared each-block fragment, deferred `{#if}` branches,
  `<svelte:boundary>` pending content); eager creation would leak a Rust node per abandoned render.
- **`remove()` never destroys.** Svelte removes and re-inserts the same node in consecutive
  statements, so removal only detaches and enqueues; `destroyElement` runs at commit time for nodes
  that are still detached.

Mutations queue up and ship as a single `applyBatch` call, committed before each `tick()` and
immediately after each event.

## Styling

Svelte hands the renderer the `style` attribute as CSS **text**, never as an object, so `src/style.js`
translates it into GPUI's camelCase `StyleDesc`: `background-color` → `backgroundColor`, `16px` → `16`,
while `50%`, `auto` and `#1e1e2e` stay strings. `style:` directives and dynamic values work normally.

GPUI's nested pseudo-styles have no CSS-text spelling, so they get their own attributes:

```svelte
<div style="background-color: #313244" hover="background-color: #45475a" active="opacity: 0.8">
```

`class` is inert — GPUI has no CSS engine.

## Limits

Compile errors under `customRenderer` (enforced by the Svelte compiler): `{@html}`, `bind:` on
elements, `transition:` / `in:` / `out:`, `animate:`, legacy `on:`, `<svelte:head|window|document|body>`,
and `css: 'injected'`. Component `bind:` and `bind:this` are fine.

Beyond that:

- **No event bubbling.** GPUI dispatches straight to the element, so a parent's `onclick` does not
  fire for a child's click. (Same limitation as `@gpuix/react`.)
- **Whitespace-only text is dropped.** GPUI lays out with flex, not inline text flow, so an
  inter-element space would render as a stray row. The cost is that a deliberate space between two
  expressions (`{a} {b}`) is lost — put it inside one expression instead.
- **Unknown tags degrade to `div`** with a warning. GPUI has no `<span>`, `<p>` or `<section>`.
- Only the events in `src/events.js` exist; anything else is silently not registered.

## Tests

```
bun --conditions custom-renderer --conditions development test/smoke.js     # mount + interact + screenshot
bun --conditions custom-renderer --conditions development test/reorder.js   # keyed {#each} projection
```

Both run against `TestGpuixRenderer`, which drives the real Metal pipeline without opening a window.
`reorder.js` is the important one: keyed reordering re-inserts nodes before anchors that have no GPUI
presence, and it is the one place the projection can silently produce the wrong native order.
