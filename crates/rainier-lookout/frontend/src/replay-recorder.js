// The recorder Lookout's browser script loads for session replay
// (LOOKOUT_RUM_REPLAY) — built on its own into assets/dist/rum-replay.js, so
// a page that never records never downloads it. rrweb's recorder, nothing
// else: rum.js decides when to start it and what to keep.
import { record } from '@rrweb/record'

window.LookoutReplay = { record }
