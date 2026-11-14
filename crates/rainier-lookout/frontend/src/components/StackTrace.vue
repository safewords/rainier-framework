<script setup>
import { computed, ref } from 'vue'

const props = defineProps({
  // The application's frames, innermost first: [{ function, file?, line? }].
  frames: { type: Array, default: () => [] },
  // The raw Rust backtrace, when there is one.
  backtrace: { type: String, default: '' },
})

// The framework, the runtime and the standard library: not where the bug is.
// Mirrors FOREIGN_FRAMES in watchers/exceptions.rs.
const FOREIGN = [
  'std::', 'core::', 'alloc::', 'tokio::', 'futures', 'hyper', 'tower', 'backtrace::', 'rainier_',
  'async_trait', '<', '__rust', 'rust_begin_unwind', 'test::', 'tracing', 'pin_project', 'sea_orm',
  'sqlx', 'serde', 'hashbrown', 'anyhow::',
]

const open = ref(false)

const list = computed(() =>
  (Array.isArray(props.frames) ? props.frames : [])
    .filter((f) => f && typeof f === 'object')
    .map((f) => ({
      function: typeof f.function === 'string' ? f.function : String(f.function ?? '?'),
      file: typeof f.file === 'string' ? f.file : null,
      line: f.line ?? null,
    })),
)

/** Each line of the raw backtrace, marked when it belongs to an application frame. */
const lines = computed(() => {
  if (!props.backtrace) return []
  let app = false
  return props.backtrace.split('\n').map((raw) => {
    const text = raw.trimEnd()
    const trimmed = text.trim()
    const symbol = trimmed.match(/^\d+:\s+(.*)$/)
    if (symbol) {
      const name = symbol[1].trim()
      app = name.includes('::') && !FOREIGN.some((p) => name.startsWith(p))
      return { text, app, at: false }
    }
    if (trimmed.startsWith('at ')) return { text, app, at: true }
    app = false
    return { text, app: false, at: false }
  })
})

const hasAny = computed(() => list.value.length > 0 || lines.value.length > 0)
</script>

<template>
  <div>
    <p v-if="!hasAny" class="text-sm text-zinc-500 dark:text-zinc-400">
      No stack — the binary was built without symbols or RUST_BACKTRACE is unset.
    </p>
    <template v-else>
      <ol v-if="list.length" class="overflow-hidden rounded-lg border border-zinc-200 dark:border-zinc-800">
        <li
          v-for="(frame, i) in list"
          :key="i"
          class="flex min-w-0 flex-col gap-0.5 border-b border-zinc-100 px-3 py-2 last:border-b-0 sm:flex-row sm:items-baseline sm:gap-3 dark:border-zinc-800"
          :class="i === 0 ? 'bg-rose-50/70 dark:bg-rose-500/10' : ''"
        >
          <span class="w-5 shrink-0 text-[11px] text-zinc-400 tabular-nums dark:text-zinc-500">{{ i }}</span>
          <span
            class="min-w-0 font-mono text-[12.5px] break-all"
            :class="i === 0 ? 'font-semibold text-rose-800 dark:text-rose-300' : 'text-zinc-800 dark:text-zinc-200'"
          >{{ frame.function }}</span>
          <span v-if="frame.file" class="min-w-0 font-mono text-xs break-all text-zinc-500 sm:ml-auto sm:text-right dark:text-zinc-400">
            {{ frame.file }}<template v-if="frame.line !== null">:{{ frame.line }}</template>
          </span>
        </li>
      </ol>
      <p v-else class="text-sm text-zinc-500 dark:text-zinc-400">No application frames — the stack is all framework and runtime.</p>

      <div v-if="lines.length" class="mt-2">
        <button type="button" class="lk-btn" :aria-expanded="open" @click="open = !open">
          {{ open ? 'Hide full backtrace' : 'Full backtrace' }}
        </button>
        <pre
          v-if="open"
          class="mt-2 max-h-[28rem] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed whitespace-pre-wrap break-all dark:bg-black/40"
        ><span
          v-for="(l, i) in lines"
          :key="i"
          class="block"
          :class="l.app ? (l.at ? 'text-indigo-300' : 'font-semibold text-zinc-50') : 'text-zinc-500'"
        >{{ l.text || ' ' }}</span></pre>
      </div>
    </template>
  </div>
</template>
