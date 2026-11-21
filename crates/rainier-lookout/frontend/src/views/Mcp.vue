<script setup>
import { computed, ref } from 'vue'

import Pill from '../components/Pill.vue'
import { config } from '../config.js'

const mcp = config.mcp

// Where a client connects: this page's own origin, which is the
// application's, and the server's path.
const url = computed(() => (mcp ? `${window.location.origin}${mcp.path}` : ''))

// A name a client accepts: lower case, letters, digits and dashes.
const name = computed(() => {
  const raw = (mcp && mcp.name) || 'app'
  const slug = raw.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '')
  return slug || 'app'
})

const needsToken = computed(() => !!mcp && !mcp.open)
const tokenVar = 'MCP_TOKEN'
const lookoutTools = computed(() => (mcp ? mcp.tools.filter((t) => t.startsWith('lookout_')) : []))
const otherTools = computed(() => (mcp ? mcp.tools.filter((t) => !t.startsWith('lookout_')) : []))

const claudeCode = computed(() => {
  const header = needsToken.value ? ` \\\n  --header "Authorization: Bearer $${tokenVar}"` : ''
  return `claude mcp add --transport http ${name.value} ${url.value}${header}`
})

const projectFile = computed(() => {
  const server = { type: 'http', url: url.value }
  if (needsToken.value) server.headers = { Authorization: `Bearer \${${tokenVar}}` }
  return JSON.stringify({ mcpServers: { [name.value]: server } }, null, 2)
})

const curl = computed(() => {
  const auth = needsToken.value ? ` \\\n  -H "Authorization: Bearer $${tokenVar}"` : ''
  return (
    `curl -sS -X POST ${url.value} \\\n  -H 'Content-Type: application/json'${auth} \\\n` +
    `  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'`
  )
})

const copied = ref('')
async function copy(key, text) {
  try {
    await navigator.clipboard.writeText(text)
    copied.value = key
    setTimeout(() => {
      if (copied.value === key) copied.value = ''
    }, 1500)
  } catch {
    copied.value = ''
  }
}

const enableEnv = `MCP_ENABLED=true
MCP_TOKENS=<a random string of at least 32 characters>`
const enableToml = `rainier-framework = { version = "2", features = ["lookout", "mcp"] }`
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">MCP</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        Connect an AI assistant to this application's Model Context Protocol server, so it can read what Lookout
        records — the slowest routes, a request's waterfall, the open cases — and explain it.
      </p>
    </div>

    <section v-if="!mcp" class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">No MCP server here</h2>
        <Pill tone="neutral">off</Pill>
      </header>
      <div class="space-y-3 px-4 py-3 text-sm">
        <p>This application is not serving one. To turn it on, build with the framework's <code class="font-mono">mcp</code> feature:</p>
        <pre class="overflow-x-auto rounded-md bg-zinc-100 p-3 font-mono text-xs dark:bg-zinc-950">{{ enableToml }}</pre>
        <p>and set, in <code class="font-mono">.env</code> or the process environment:</p>
        <pre class="overflow-x-auto rounded-md bg-zinc-100 p-3 font-mono text-xs dark:bg-zinc-950">{{ enableEnv }}</pre>
        <p class="text-zinc-500 dark:text-zinc-400">
          Restart, and this page shows the address and the command to connect. With Lookout on, its tools join the
          server unless <code class="font-mono">MCP_LOOKOUT=false</code>.
        </p>
      </div>
    </section>

    <template v-else>
      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Server</h2>
          <Pill tone="ok">on</Pill>
        </header>
        <dl class="divide-y divide-zinc-100 dark:divide-zinc-800">
          <div class="flex flex-wrap items-center justify-between gap-2 px-4 py-2.5 text-sm">
            <dt class="text-zinc-500 dark:text-zinc-400">Address</dt>
            <dd class="flex min-w-0 items-center gap-2">
              <code class="truncate font-mono text-xs">{{ url }}</code>
              <button type="button" class="lk-btn h-7 px-2 text-xs" @click="copy('url', url)">
                {{ copied === 'url' ? 'Copied' : 'Copy' }}
              </button>
            </dd>
          </div>
          <div class="flex flex-wrap justify-between gap-2 px-4 py-2.5 text-sm">
            <dt class="text-zinc-500 dark:text-zinc-400">Name</dt>
            <dd class="font-medium">{{ mcp.name }}</dd>
          </div>
          <div class="flex flex-wrap justify-between gap-2 px-4 py-2.5 text-sm">
            <dt class="text-zinc-500 dark:text-zinc-400">Transport</dt>
            <dd class="font-medium">Streamable HTTP, stateless</dd>
          </div>
          <div class="flex flex-wrap justify-between gap-2 px-4 py-2.5 text-sm">
            <dt class="text-zinc-500 dark:text-zinc-400">Access here</dt>
            <dd class="font-medium">
              <Pill v-if="mcp.open" tone="warn">open — local environment</Pill>
              <Pill v-else-if="mcp.tokens" tone="ok">bearer token</Pill>
              <Pill v-else tone="bad">no token configured</Pill>
            </dd>
          </div>
        </dl>
        <div class="border-t border-zinc-100 px-4 py-3 text-sm text-zinc-600 dark:border-zinc-800 dark:text-zinc-300">
          <p v-if="mcp.open">
            Anyone who can reach this address may connect, without a token, because the application runs with
            <code class="font-mono">APP_ENV=local</code>. Set <code class="font-mono">MCP_OPEN_LOCALLY=false</code> to require one.
          </p>
          <p v-else-if="mcp.tokens">
            A client sends <code class="font-mono">Authorization: Bearer &lt;token&gt;</code> with one of the deployment's
            <code class="font-mono">MCP_TOKENS</code>. The token is never shown here: ask whoever runs this deployment, and keep it
            in an environment variable — the commands below read <code class="font-mono">{{ tokenVar }}</code>. Anything else
            is answered 404.
          </p>
          <p v-else>
            Outside the local environment and with no <code class="font-mono">MCP_TOKENS</code> set, only the application's own
            access check (<code class="font-mono">with_mcp_access</code>) can let a client in. Set
            <code class="font-mono">MCP_TOKENS</code> to connect with a token.
          </p>
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Claude Code</h2>
          <button type="button" class="lk-btn ml-auto h-7 px-2 text-xs" @click="copy('cc', claudeCode)">
            {{ copied === 'cc' ? 'Copied' : 'Copy' }}
          </button>
        </header>
        <div class="space-y-2 px-4 py-3 text-sm">
          <p v-if="needsToken">
            With the token in <code class="font-mono">{{ tokenVar }}</code>, run:
          </p>
          <p v-else>Run:</p>
          <pre class="overflow-x-auto rounded-md bg-zinc-100 p-3 font-mono text-xs dark:bg-zinc-950">{{ claudeCode }}</pre>
          <p class="text-zinc-500 dark:text-zinc-400">
            Then <code class="font-mono">/mcp</code> in Claude Code lists it, and the prompts below appear as commands.
          </p>
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">A project's <code class="font-mono">.mcp.json</code></h2>
          <button type="button" class="lk-btn ml-auto h-7 px-2 text-xs" @click="copy('json', projectFile)">
            {{ copied === 'json' ? 'Copied' : 'Copy' }}
          </button>
        </header>
        <div class="space-y-2 px-4 py-3 text-sm">
          <p>
            To share the server with everyone working in a repository, check this in at its root.
            <template v-if="needsToken">The token stays out of it: <code class="font-mono">${{ '{' }}{{ tokenVar }}{{ '}' }}</code> is read from each person's environment.</template>
          </p>
          <pre class="overflow-x-auto rounded-md bg-zinc-100 p-3 font-mono text-xs dark:bg-zinc-950">{{ projectFile }}</pre>
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Any other client, or a quick check</h2>
          <button type="button" class="lk-btn ml-auto h-7 px-2 text-xs" @click="copy('curl', curl)">
            {{ copied === 'curl' ? 'Copied' : 'Copy' }}
          </button>
        </header>
        <div class="space-y-2 px-4 py-3 text-sm">
          <p>
            Any client that speaks Streamable HTTP connects with the address<template v-if="needsToken"> and the
            <code class="font-mono">Authorization</code> header</template>. This lists the tools, to check the address and the token
            work:
          </p>
          <pre class="overflow-x-auto rounded-md bg-zinc-100 p-3 font-mono text-xs dark:bg-zinc-950">{{ curl }}</pre>
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">What it offers</h2></header>
        <div class="space-y-4 px-4 py-3 text-sm">
          <div>
            <h3 class="lk-th mb-2">Lookout tools</h3>
            <p v-if="!lookoutTools.length" class="text-zinc-500 dark:text-zinc-400">
              None — <code class="font-mono">MCP_LOOKOUT=false</code> leaves them out.
            </p>
            <ul v-else class="flex flex-wrap gap-1.5">
              <li v-for="tool in lookoutTools" :key="tool"><Pill tone="accent">{{ tool }}</Pill></li>
            </ul>
          </div>
          <div v-if="otherTools.length">
            <h3 class="lk-th mb-2">The application's tools</h3>
            <ul class="flex flex-wrap gap-1.5">
              <li v-for="tool in otherTools" :key="tool"><Pill tone="info">{{ tool }}</Pill></li>
            </ul>
          </div>
          <div v-if="mcp.prompts.length">
            <h3 class="lk-th mb-2">Prompts</h3>
            <ul class="flex flex-wrap gap-1.5">
              <li v-for="prompt in mcp.prompts" :key="prompt"><Pill>{{ prompt }}</Pill></li>
            </ul>
            <p class="mt-2 text-zinc-500 dark:text-zinc-400">
              Each is a recipe: which tools to call, in what order, and what to report. Try
              <code class="font-mono">lookout_health</code> first.
            </p>
          </div>
        </div>
      </section>

      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        A token reads everything Lookout records, and lets a client resolve or ignore cases: treat it like a database
        password. Recorded entries hold what users sent — URLs, query text, exception messages — so a client that lets
        the model act on what it reads should still ask first.
      </p>
    </template>
  </div>
</template>
