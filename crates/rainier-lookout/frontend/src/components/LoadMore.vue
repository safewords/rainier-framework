<script setup>
defineProps({ loading: Boolean })
defineEmits(['more'])
</script>

<template>
  <div class="flex justify-center border-t border-zinc-200 px-4 py-3 dark:border-zinc-800">
    <button type="button" class="lk-btn" :disabled="loading" @click="$emit('more')">
      {{ loading ? 'Loading…' : 'Load older' }}
    </button>
  </div>
</template>
