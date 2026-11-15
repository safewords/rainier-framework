<script setup>
// Who made a request: what the application described with
// `Lookout::describe_user` (LOOKOUT_CAPTURE_USER), or just the id and guard.
import { computed } from 'vue'

const props = defineProps({
  user: { type: Object, default: null },
  userId: { type: String, default: null },
  guard: { type: String, default: null },
})

const id = computed(() => (props.user && props.user.id) || props.userId)
const name = computed(() => (props.user && props.user.name) || null)
const initials = computed(() => {
  const source = name.value || (props.user && props.user.email) || id.value || '?'
  const words = String(source).split(/[\s@._-]+/).filter(Boolean)
  return (words.length > 1 ? words[0][0] + words[1][0] : String(source).slice(0, 2)).toUpperCase()
})
const extra = computed(() => {
  const e = props.user && props.user.extra
  return e && typeof e === 'object' ? Object.entries(e) : []
})
const show = (v) => (v === null || v === undefined ? '—' : typeof v === 'object' ? JSON.stringify(v) : String(v))
</script>

<template>
  <section class="lk-card">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">User</h2>
      <RouterLink
        v-if="id"
        :to="{ name: 'batches', query: { kind: 'request', user: id } }"
        class="ml-auto text-xs text-indigo-600 hover:underline dark:text-indigo-400"
      >Their requests</RouterLink>
    </header>
    <div class="flex flex-wrap items-start gap-4 p-4">
      <!-- Initials rather than the avatar itself: the dashboard loads nothing
           from other hosts, which would tell them who is looking. -->
      <div
        class="grid size-11 shrink-0 place-items-center rounded-full bg-indigo-100 text-sm font-semibold text-indigo-700 dark:bg-indigo-500/20 dark:text-indigo-300"
        aria-hidden="true"
      >{{ initials }}</div>
      <dl class="grid min-w-0 flex-1 grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3">
        <div class="min-w-0">
          <dt class="lk-th">Name</dt>
          <dd class="mt-0.5 text-sm break-words">{{ show(name) }}</dd>
        </div>
        <div class="min-w-0">
          <dt class="lk-th">Email</dt>
          <dd class="mt-0.5 text-sm break-words">{{ show(user && user.email) }}</dd>
        </div>
        <div class="min-w-0">
          <dt class="lk-th">Id</dt>
          <dd class="mt-0.5 font-mono text-[12.5px] break-words">{{ show(id) }}</dd>
        </div>
        <div v-if="guard" class="min-w-0">
          <dt class="lk-th">Guard</dt>
          <dd class="mt-0.5 text-sm">{{ guard }}</dd>
        </div>
        <div v-if="user && user.avatar_url" class="min-w-0">
          <dt class="lk-th">Avatar</dt>
          <dd class="mt-0.5 truncate text-sm">
            <a :href="user.avatar_url" target="_blank" rel="noopener noreferrer" class="text-indigo-600 hover:underline dark:text-indigo-400">open</a>
          </dd>
        </div>
        <div v-for="[key, value] in extra" :key="key" class="min-w-0">
          <dt class="lk-th">{{ key }}</dt>
          <dd class="mt-0.5 text-sm break-words">{{ show(value) }}</dd>
        </div>
      </dl>
    </div>
  </section>
</template>
