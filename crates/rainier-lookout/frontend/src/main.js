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
import Session from './views/Session.vue'
import Sessions from './views/Sessions.vue'
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
    // `/issues/:type/:id`; `/issues/:type?key=…`, the earlier form, still works.
    { path: '/issues/:type/:id?', name: 'issue', component: Issue, props: true },
    { path: '/jobs', name: 'jobs', component: Timing, props: { type: 'job' } },
    { path: '/requests', name: 'requests', component: Timing, props: { type: 'request' } },
    { path: '/cases', name: 'cases', component: Cases },
    { path: '/cases/:id', name: 'case', component: CaseDetail, props: true },
    // ?q=<text>
    { path: '/search', name: 'search', component: Search },
    { path: '/sessions', name: 'sessions', component: Sessions },
    // One request, job, page view or API call, at a path that says which.
    // `/batches/:id` still opens any of them: links already shared, and
    // places that know only the id.
    {
      path: '/:kind(requests|jobs|commands|tasks|other|page-views|api-calls)/:id',
      name: 'item',
      component: BatchDetail,
      props: (route) => ({ id: route.params.id }),
    },
    { path: '/sessions/:id', name: 'session', component: Session, props: true },
    { path: '/:rest(.*)*', name: 'not-found', component: NotFound },
  ],
  scrollBehavior: (to, from, saved) => saved || { top: 0 },
})

createApp(App).use(router).mount('#lookout')
