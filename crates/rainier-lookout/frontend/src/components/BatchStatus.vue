<script setup>
import Pill from './Pill.vue'
import { statusTone } from '../format.js'

defineProps({ batch: { type: Object, required: true } })
</script>

<template>
  <Pill v-if="batch.status" :tone="statusTone(batch.status)">{{ batch.status }}</Pill>
  <Pill v-else-if="batch.has_exception" tone="bad">failed</Pill>
  <Pill v-else tone="ok">done</Pill>
</template>
