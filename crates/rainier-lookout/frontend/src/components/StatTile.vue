<script setup>
// One headline number: a label, the value, and a line of context under it.
// The default slot sits under the value (a sparkline), so the number keeps
// the full width of the tile. The type shrinks until the whole value fits.
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = defineProps({
  label: { type: String, required: true },
  value: { type: [String, Number], default: '—' },
  hint: { type: String, default: '' },
  tone: { type: String, default: 'neutral' },
  loading: Boolean,
})

const tones = {
  neutral: 'text-zinc-900 dark:text-zinc-100',
  bad: 'text-rose-600 dark:text-rose-400',
  warn: 'text-amber-600 dark:text-amber-400',
  good: 'text-emerald-600 dark:text-emerald-400',
  accent: 'text-indigo-600 dark:text-indigo-400',
}

const valueEl = ref(null)
let observer = null

/** Shrink from 1.5rem until the whole value sits inside the tile. */
async function fit() {
  const node = valueEl.value
  if (!node) return
  let size = 24
  node.style.fontSize = `${size}px`
  await nextTick()
  while (size > 12 && node.scrollWidth > node.clientWidth + 1) {
    size -= 1
    node.style.fontSize = `${size}px`
  }
}

onMounted(() => {
  fit()
  const box = valueEl.value && valueEl.value.parentElement
  if (box && typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver(() => fit())
    observer.observe(box)
  }
})
onBeforeUnmount(() => observer && observer.disconnect())
watch(() => [props.value, props.loading], () => nextTick(fit))
</script>

<template>
  <div class="lk-card min-w-0 px-4 py-3">
    <div class="lk-th" :title="label">{{ label }}</div>
    <div
      ref="valueEl"
      class="mt-1 overflow-hidden font-semibold tabular-nums leading-none whitespace-nowrap"
      :class="[tones[tone] || tones.neutral, loading ? 'animate-pulse opacity-40' : '']"
    >{{ loading ? '—' : value }}</div>
    <div v-if="$slots.default" class="mt-2"><slot /></div>
    <div v-if="hint" class="mt-1 text-xs leading-snug text-zinc-500 dark:text-zinc-400">{{ hint }}</div>
  </div>
</template>
