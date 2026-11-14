<script setup>
import { computed } from 'vue'

const props = defineProps({ error: { type: Object, default: null } })
const messages = computed(() => {
  const errors = props.error && props.error.errors
  if (!errors || typeof errors !== 'object') return []
  return Object.values(errors).flat().map(String)
})
</script>

<template>
  <div
    v-if="error"
    role="alert"
    class="rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-800 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300"
  >
    <p class="font-semibold">{{ error.message }}</p>
    <ul v-if="messages.length" class="mt-1 list-disc pl-5">
      <li v-for="(message, i) in messages" :key="i">{{ message }}</li>
    </ul>
  </div>
</template>
