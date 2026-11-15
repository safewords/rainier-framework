<script setup>
// What made a request, from the headers it carried (LOOKOUT_CAPTURE_CLIENT):
// address, browser, system, device, the page it came from.
import { computed } from 'vue'

const props = defineProps({ client: { type: Object, required: true } })

const versioned = (o) => (o && o.name ? [o.name, o.version].filter(Boolean).join(' ') : null)
const facts = computed(() => {
  const c = props.client || {}
  return [
    ['Address', c.ip, true],
    ['Browser', versioned(c.browser)],
    ['System', versioned(c.os)],
    ['Device', c.model ? `${c.device} · ${c.model}` : c.device],
    ['Language', c.language],
    ['Origin', c.origin, true],
    ['Page', c.referer, true],
  ].filter(([, v]) => v !== undefined && v !== null && v !== '')
})
</script>

<template>
  <section class="lk-card">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">Client</h2>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">from the request's headers</span>
    </header>
    <dl class="grid grid-cols-2 gap-x-6 gap-y-3 p-4 sm:grid-cols-3 lg:grid-cols-4">
      <div v-for="[name, value, mono] in facts" :key="name" class="min-w-0">
        <dt class="lk-th">{{ name }}</dt>
        <dd class="mt-0.5 text-sm break-words" :class="mono ? 'font-mono text-[12.5px]' : ''">{{ value }}</dd>
      </div>
    </dl>
    <p v-if="client.user_agent" class="border-t border-zinc-100 px-4 py-2.5 font-mono text-[11.5px] break-words text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
      {{ client.user_agent }}
    </p>
  </section>
</template>
