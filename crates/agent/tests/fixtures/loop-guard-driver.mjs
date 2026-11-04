// Exercise the shipped hook in the fake Pi, rather than reimplementing its policy in Python.
import { registerLoopGuard } from '../../runtime/loop-guard.mjs';
import { createInterface } from 'node:readline';
const handlers = new Map();
let entries = [], sequence = 0;
registerLoopGuard({
  on(type, handler) { handlers.set(type, handler); },
  appendEntry(customType, data) { entries.push({ type: 'custom', id: `guard-${++sequence}`, customType, data }); },
});
for await (const line of createInterface({ input: process.stdin })) {
  const event = JSON.parse(line);
  entries = [];
  const decision = await handlers.get(event.type)?.(event);
  process.stdout.write(JSON.stringify({ decision, entries }) + '\n');
}
