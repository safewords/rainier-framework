import { createApp } from 'vue'
import { createRouter, createWebHistory } from 'vue-router'

import './style.css'
import App from './App.vue'
import { config } from './config.js'
import Dashboard from './views/Dashboard.vue'
import Batches from './views/Batches.vue'
import BatchDetail from './views/BatchDetail.vue'
import Entries from './views/Entries.vue'
import Status from './views/Status.vue'
import Issue from './views/Issue.vue'
import Timing from './views/Timing.vue'
import Cases from './views/Cases.vue'
import CaseDetail from './views/CaseDetail.vue'
import Search from './views/Search.vue'
import NotFound from './views/NotFound.vue'

const router = createRouter({
  history: createWebHistory(`${config.base}/`),
  routes: [
    { path: '/', name: 'dashboard', component: Dashboard },
    { path: '/batches', name: 'batches', component: Batches },
    { path: '/batches/:id', name: 'batch', component: BatchDetail, props: true },
    { path: '/entries/:type', name: 'entries', component: Entries, props: true },
    { path: '/status', name: 'status', component: Status },
    // One family of one type: ?key=<family key>
    { path: '/issues/:type', name: 'issue', component: Issue, props: true },
    { path: '/jobs', name: 'jobs', component: Timing, props: { type: 'job' } },
    { path: '/requests', name: 'requests', component: Timing, props: { type: 'request' } },
    { path: '/cases', name: 'cases', component: Cases },
    { path: '/cases/:id', name: 'case', component: CaseDetail, props: true },
    // ?q=<text>
    { path: '/search', name: 'search', component: Search },
    { path: '/:rest(.*)*', name: 'not-found', component: NotFound },
  ],
  scrollBehavior: (to, from, saved) => saved || { top: 0 },
})

createApp(App).use(router).mount('#lookout')
