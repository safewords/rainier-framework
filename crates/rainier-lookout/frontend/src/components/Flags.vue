<script setup>
import Pill from './Pill.vue'
import { isNPlusOne } from '../format.js'

defineProps({
  exception: Boolean,
  slow: Boolean,
  tags: { type: Array, default: () => [] },
})
</script>

<template>
  <span class="inline-flex flex-wrap gap-1">
    <Pill v-if="exception" tone="bad">exception</Pill>
    <Pill v-if="slow" tone="warn">slow</Pill>
    <Pill v-if="isNPlusOne(tags)" tone="warn">N+1</Pill>
    <Pill v-if="tags.includes('failed')" tone="bad">failed</Pill>
  </span>
</template>
