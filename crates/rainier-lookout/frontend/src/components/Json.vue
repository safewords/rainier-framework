<script setup>
import { computed } from 'vue'

const props = defineProps({ value: { default: null } })
const LIMIT = 64 * 1024
const text = computed(() => {
  let out
  try {
    out = JSON.stringify(props.value, null, 2)
  } catch {
    out = String(props.value)
  }
  if (out === undefined) out = 'null'
  return out.length > LIMIT ? `${out.slice(0, LIMIT)}\n… (truncated — the API has the whole entry)` : out
})
</script>

<template>
  <pre
    class="max-h-[28rem] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed whitespace-pre-wrap break-words text-zinc-100 dark:bg-black/40"
  >{{ text }}</pre>
</template>
