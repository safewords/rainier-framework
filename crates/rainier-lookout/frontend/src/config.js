// What the server wrote into the shell: where the dashboard is mounted, where
// its API is, and a little context. Read as JSON from a data element — never
// evaluated.
function read() {
  const element = document.getElementById('lookout-config')
  try {
    return JSON.parse(element ? element.textContent : '{}')
  } catch {
    return {}
  }
}

const raw = read()

export const config = {
  base: typeof raw.base === 'string' ? raw.base : '',
  api: typeof raw.api === 'string' ? raw.api : '/api',
  environment: typeof raw.environment === 'string' ? raw.environment : '',
  store: typeof raw.store === 'string' ? raw.store : '',
  // The server's insight thresholds (LOOKOUT_INSIGHTS_*); a viewer may
  // override each one per request.
  insights: insights(raw.insights),
  // Whether "Live" may stream (LOOKOUT_STREAM); otherwise it polls.
  stream: raw.stream !== false,
  // The application's MCP server, for the MCP page: `null` when there is none.
  mcp: mcp(raw.mcp),
}

function mcp(value) {
  if (!value || typeof value !== 'object' || typeof value.path !== 'string') return null
  const names = (list) => (Array.isArray(list) ? list.filter((n) => typeof n === 'string') : [])
  return {
    path: value.path,
    name: typeof value.name === 'string' ? value.name : '',
    open: value.open === true,
    tokens: value.tokens === true,
    tools: names(value.tools),
    prompts: names(value.prompts),
  }
}

function insights(value) {
  const v = value && typeof value === 'object' ? value : {}
  const whole = (n, fallback) => (Number.isFinite(n) && n > 0 ? Math.floor(n) : fallback)
  return {
    enabled: v.enabled !== false,
    window: whole(v.window_minutes, 60),
    min: whole(v.min_count, 5),
    change: whole(v.change_percent, 50),
    forecast: whole(v.forecast_buckets, 6),
  }
}
