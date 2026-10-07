import { createApp, h, nextTick, reactive } from 'vue';
import VirtualEntries from '../src/VirtualEntries.vue';
import type { EntryDto } from '../src/types';
const host = window as unknown as Record<string, unknown>;
const plan = reactive({ planId: 'initial-large', count: '150' });
const counts = new Map<string, number>([[plan.planId, 150]]);
const requests: { planId: string; offset: number; limit: number }[] = [];
const held = new Set<string>();
const pending: { planId: string; resolve: () => void }[] = [];
host.__planEntriesFixture = (args: { planId: string; offset: number; limit: number }) => {
  requests.push({ ...args });
  return new Promise<EntryDto[]>(resolve => {
    const finish = () => resolve(Array.from({ length: Math.max(0, Math.min(args.limit, (counts.get(args.planId) ?? 0) - args.offset)) }, (_, i) => ({
      source: 'synthetic-only', relative_path: `${args.planId}/${String(args.offset + i).padStart(3, '0')}`, kind: 'File', bytes: '1'
    })));
    if (held.has(args.planId)) pending.push({ planId: args.planId, resolve: finish });
    else finish();
  });
};
const container = document.createElement('div');
container.id = 'isolated-virtual-regression';
container.style.cssText = 'position:fixed;inset:10px;background:white;z-index:10000;padding:20px;overflow:auto';
document.body.appendChild(container);
const shadow = container.attachShadow({ mode: 'open' });
const root = document.createElement('div');
shadow.appendChild(root);
const app = createApp({ render: () => h(VirtualEntries, plan) });
app.mount(root);
host.__scrollFixture = {
  requests,
  async setPlan(planId: string, count: number) { counts.set(planId, count); plan.planId = planId; plan.count = String(count); await nextTick(); },
  hold(planId: string) { held.add(planId); },
  release(planId: string) { held.delete(planId); for (const item of pending.filter(x => x.planId === planId)) item.resolve(); },
  remove() { app.unmount(); container.remove(); delete host.__scrollFixture; delete host.__planEntriesFixture; }
};
