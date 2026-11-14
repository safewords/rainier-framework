<script setup>
import { computed, inject } from 'vue'
import { absolute, ago } from '../format.js'

const props = defineProps({ at: { type: String, required: true } })
const now = inject('now')
const text = computed(() => ago(props.at, now ? now.value : Date.now()))
</script>

<template>
  <time :datetime="at" :title="absolute(at)" class="whitespace-nowrap">{{ text }}</time>
</template>
