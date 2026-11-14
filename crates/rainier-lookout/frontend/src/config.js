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
}
