// Put the MIT notices of what each bundle includes at its top: the licence
// asks that they travel with every copy, and the minifier drops comments.
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const dist = (name) => fileURLToPath(new URL(`../../assets/dist/${name}`, import.meta.url))
const RRWEB = 'rrweb 2.2.0, MIT License, Copyright (c) 2018 Contributors (https://github.com/rrweb-io/rrweb/graphs/contributors) and SmartX Inc.'
const notices = {
  'lookout.js': [
    'Vue, MIT License, Copyright (c) 2018-present Yuxi (Evan) You and Vue contributors.',
    'Vue Router, MIT License, Copyright (c) 2019-present Eduardo San Martin Morote.',
    RRWEB,
  ],
  'rum-replay.js': [RRWEB],
}
for (const [file, lines] of Object.entries(notices)) {
  const path = dist(file)
  const body = readFileSync(path, 'utf8').replace(/^\/\*! Includes[\s\S]*?\*\/\n/, '')
  writeFileSync(path, `/*! Includes:\n${lines.map((l) => ` * ${l}`).join('\n')}\n */\n${body}`)
}
