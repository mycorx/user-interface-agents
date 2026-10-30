// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

import { mount } from 'svelte';
import AlertCard from './AlertCard.svelte';

const target = document.getElementById('app');
if (!target) {
  throw new Error('missing #app mount point in alert.html');
}

const app = mount(AlertCard, { target });

export default app;
