<script setup>
import { computed, onMounted, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import BatchList from '../components/BatchList.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LoadMore from '../components/LoadMore.vue'
import { api } from '../api.js'
import { KINDS, TYPES } from '../format.js'
import { usePolling } from '../live.js'

const PAGE = 50
const route = useRoute()
const router = useRouter()

const kind = computed(() => (typeof route.query.kind === 'string' ? route.query.kind : 'request'))
const label = computed(() => (KINDS.find((k) => k.key === kind.value) || { label: 'Batches' }).label)

const text = (key) => (typeof route.query[key] === 'string' ? route.query[key] : '')
const form = reactive({
  route: text('route'),
  user: text('user'),
  tag: text('tag'),
  type: text('type'),
  exception: text('exception') === '1',
  slow: text('slow') === '1',
})
const filters = computed(() => ({
  kind: kind.value,
  route: text('route'),
  user: text('user'),
  tag: text('tag'),
  type: text('type'),
  exception: text('exception') === '1',
  slow: text('slow') === '1',
  trace: text('trace'),
}))
const filtered = computed(() =>
  ['route', 'user', 'tag', 'type', 'exception', 'slow', 'trace'].some((k) => filters.value[k]),
)

const batches = ref([])
const next = ref(null)
const loading = ref(true)
const more = ref(false)
const error = ref(null)

function apply() {
  const query = { kind: kind.value }
  for (const key of ['route', 'user', 'tag', 'type']) if (form[key].trim()) query[key] = form[key].trim()
  if (form.exception) query.exception = '1'
  if (form.slow) query.slow = '1'
  router.replace({ query })
}
function reset() {
  router.replace({ query: { kind: kind.value } })
}

async function load() {
  try {
    const page = await api.batches({ ...filters.value, limit: PAGE })
    batches.value = page.data
    next.value = page.next
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

async function loadMore() {
  if (!next.value) return
  more.value = true
  try {
    const page = await api.batches({ ...filters.value, limit: PAGE, before: next.value.before })
    const seen = new Set(batches.value.map((b) => b.id))
    batches.value = batches.value.concat(page.data.filter((b) => !seen.has(b.id)))
    next.value = page.next
  } catch (e) {
    error.value = e
  } finally {
    more.value = false
  }
}

// Live: fetch the newest page and put anything new on top, keeping what
// "Load older" already brought in.
async function refresh() {
  try {
    const page = await api.batches({ ...filters.value, limit: PAGE })
    const seen = new Set(batches.value.map((b) => b.id))
    const fresh = page.data.filter((b) => !seen.has(b.id))
    if (fresh.length) batches.value = fresh.concat(batches.value)
  } catch {
    // The next tick tries again.
  }
}

onMounted(load)
usePolling(refresh)
</script>

<template>
  <div class="space-y-4">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">{{ label }}</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">Newest first. Each row is one unit of work and everything it did.</p>
    </div>

    <nav class="-mx-4 flex gap-1 overflow-x-auto border-b border-zinc-200 px-4 sm:mx-0 sm:px-0 dark:border-zinc-800" aria-label="Kind">
      <RouterLink
        v-for="k in KINDS"
        :key="k.key"
        :to="{ name: 'batches', query: { kind: k.key } }"
        class="-mb-px border-b-2 px-3 py-2 text-sm whitespace-nowrap transition"
        :class="k.key === kind
          ? 'border-indigo-600 font-semibold text-indigo-700 dark:border-indigo-400 dark:text-indigo-300'
          : 'border-transparent text-zinc-500 hover:text-zinc-800 dark:text-zinc-400 dark:hover:text-zinc-200'"
      >{{ k.label }}</RouterLink>
    </nav>

    <form class="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:flex lg:flex-wrap lg:items-center" @submit.prevent="apply">
      <input v-model="form.route" class="lk-input lg:w-44" type="search" placeholder="Route name" aria-label="Route name" />
      <input v-model="form.user" class="lk-input lg:w-32" type="search" placeholder="User id" aria-label="User id" />
      <input v-model="form.tag" class="lk-input lg:w-32" type="search" placeholder="Tag" aria-label="Tag" />
      <select v-model="form.type" class="lk-input lg:w-44" aria-label="Has an entry of type" @change="apply">
        <option value="">Any entries</option>
        <option v-for="(name, key) in TYPES" :key="key" :value="key">Has {{ name.toLowerCase() }}</option>
      </select>
      <label class="flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-300">
        <input v-model="form.exception" type="checkbox" class="size-4 accent-indigo-600" @change="apply" /> Exceptions
      </label>
      <label class="flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-300">
        <input v-model="form.slow" type="checkbox" class="size-4 accent-indigo-600" @change="apply" /> Slow
      </label>
      <div class="col-span-2 flex gap-2 sm:col-span-3 lg:col-span-1">
        <button type="submit" class="lk-btn">Apply</button>
        <button v-if="filtered" type="button" class="lk-btn" @click="reset">Reset</button>
      </div>
    </form>

    <ErrorBox :error="error" />

    <section class="lk-card">
      <BatchList v-if="batches.length" :batches="batches" />
      <Empty v-else>{{ loading ? 'Loading…' : filtered ? 'Nothing matches those filters.' : 'Nothing recorded yet.' }}</Empty>
      <LoadMore v-if="next" :loading="more" @more="loadMore" />
    </section>
  </div>
</template>
