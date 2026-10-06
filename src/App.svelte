<script lang="ts">
  import CardDeck from './CardDeck.svelte';

  // There is only one window (PLAN.md FD2) and, since S1, only one view: the
  // card deck. Routing between HUD and Settings is no longer a branch here —
  // it is the deck's stack, because both cards are on screen together during
  // the genie transition. CardDeck owns the uia://open-settings listener
  // and the FD1 window-resize effect; App is just the mount point.
</script>

<CardDeck />
