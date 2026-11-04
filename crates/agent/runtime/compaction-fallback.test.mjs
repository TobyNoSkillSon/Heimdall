import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, existsSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { registerCompactionFallback, cutPoint, visibleTokens, lastCompaction, SOURCE_COMMAND, SUPPLY_COMMAND, SUPPLY_MS } from './compaction-fallback.mjs';

// Pi's own helpers, as heimdall.ts passes them (chars/4; one line a message).
const estimateTokens = (m) => Math.ceil(JSON.stringify(m.content ?? '').length / 4);
const deps = { estimateTokens, convertToLlm: (ms) => ms, serializeConversation: (ms) => ms.map((m) => `[${m.role}]: ${typeof m.content === 'string' ? m.content : JSON.stringify(m.content)}`).join('\n') };
function rig(clock = { t: 0 }) {
  const handlers = {}, commands = {};
  registerCompactionFallback({ on: (n, f) => { handlers[n] = f; }, registerCommand: (n, c) => { commands[n] = c.handler; } }, { ...deps, now: () => clock.t });
  return { hook: (e) => handlers.session_before_compact({ reason: 'manual', ...e }), command: (n, a) => commands[n](a), clock };
}
const msg = (id, role, text) => ({ type: 'message', id, message: { role, content: text } });
// A history of three turns: the user asks, Heimdall reads a file (call and result), Heimdall answers.
const branch = () => [
  msg('u1', 'user', 'a'.repeat(400)), msg('a1', 'assistant', [{ type: 'toolCall', id: 't1' }]), msg('r1', 'toolResult', 'b'.repeat(400)), msg('f1', 'assistant', 'c'.repeat(40)),
  msg('u2', 'user', 'd'.repeat(400)), msg('a2', 'assistant', 'e'.repeat(400)),
  { type: 'custom_message', id: 'x3', customType: 'heimdall_context', content: 'f'.repeat(40) }, msg('u3', 'user', 'g'.repeat(40)), msg('a3', 'assistant', 'h'.repeat(40)),
];
const preparation = { firstKeptEntryId: 'u3', messagesToSummarize: branch().slice(0, 7).filter((e) => e.message).map((e) => e.message), turnPrefixMessages: [], tokensBefore: 600, previousSummary: undefined };
const dir = mkdtempSync(join(tmpdir(), 'heimdall-compaction-'));

test('the source is the cut Pi chose for the failed compaction, as text, private', async () => {
  const r = rig();
  const path = join(dir, 'source.json');
  await r.command(SOURCE_COMMAND, path);
  assert.deepEqual(JSON.parse(readFileSync(path, 'utf8')), { none: true }, 'no compaction yet: nothing to summarise');
  assert.equal(await r.hook({ preparation, branchEntries: branch(), customInstructions: 'keep decisions' }), undefined, 'nothing supplied: Pi summarises itself');
  await r.command(SOURCE_COMMAND, path + ' ');
  const s = JSON.parse(readFileSync(path, 'utf8'));
  assert.equal(s.firstKeptEntryId, 'u3');
  assert.equal(s.previous, null);
  assert.equal(s.focus, 'keep decisions');
  assert.ok(s.conversation.startsWith('[user]: aaaa') && s.conversation.includes('[assistant]: eeee'));
  assert.equal(s.tokens, preparation.messagesToSummarize.reduce((n, m) => n + estimateTokens(m), 0));
  assert.equal(statSync(path).mode & 0o777, 0o600);
});

test('a supplied summary becomes the next compaction once, with Pi\'s own cut', async () => {
  const r = rig();
  await r.hook({ preparation, branchEntries: branch() });
  const path = join(dir, 'supply.json');
  writeFileSync(path, JSON.stringify({ summary: 'Luna summary', firstKeptEntryId: 'u3', previous: null, model: 'openai/gpt-6-luna' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.ok(!existsSync(path), 'the summary file is gone once read');
  assert.deepEqual(await r.hook({ preparation, branchEntries: branch() }), { compaction: { summary: 'Luna summary', firstKeptEntryId: 'u3', tokensBefore: 600, details: { heimdall: 'openai/gpt-6-luna' } } });
  assert.equal(await r.hook({ preparation, branchEntries: branch() }), undefined, 'used once');
  // Pi's own compaction inside a turn never takes one (review 28 Oct): it stays armed for Heimdall's.
  writeFileSync(path, JSON.stringify({ summary: 'mine', firstKeptEntryId: 'u3', previous: null, model: 'm' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.equal(await r.hook({ preparation, branchEntries: branch(), reason: 'threshold' }), undefined);
  assert.equal((await r.hook({ preparation, branchEntries: branch() })).compaction.summary, 'mine');
  // Pi had nothing to compact (its last entry a compaction: the hook never ran): the supply never reaches a later one.
  writeFileSync(path, JSON.stringify({ summary: 'late', firstKeptEntryId: 'u3', previous: null, model: 'm' }));
  await r.command(SUPPLY_COMMAND, path);
  r.clock.t += SUPPLY_MS + 1;
  assert.equal(await r.hook({ preparation, branchEntries: branch() }), undefined, 'Pi summarises its own compaction');
});

test('a summary for a branch that changed under it cancels: no paid call, the session as it was', async () => {
  const r = rig();
  const path = join(dir, 'stale.json');
  writeFileSync(path, JSON.stringify({ summary: 'old', firstKeptEntryId: 'u3', previous: null, model: 'm' }));
  await r.command(SUPPLY_COMMAND, path);
  const compacted = [...branch(), { type: 'compaction', id: 'c1', firstKeptEntryId: 'u3', summary: 'Pi did it' }];
  assert.deepEqual(await r.hook({ preparation, branchEntries: compacted }), { cancel: true });
  writeFileSync(path, JSON.stringify({ summary: 'old', firstKeptEntryId: 'gone', previous: null, model: 'm' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.deepEqual(await r.hook({ preparation, branchEntries: branch() }), { cancel: true });
  // A cut made before Pi compacted on its own, and a supply that cannot be read, cancel too.
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 240, tokensBefore: 1, previous: null, at: 'now' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.deepEqual(await r.hook({ preparation, branchEntries: compacted }), { cancel: true });
  writeFileSync(path, '{not json');
  await r.command(SUPPLY_COMMAND, path);
  assert.deepEqual(await r.hook({ preparation, branchEntries: branch() }), { cancel: true });
});

test('the last-resort cut keeps the newest whole turns that fit, and the previous summary', async () => {
  // Turn tokens from the end: turn 3 = 11+11+11 (x3, u3, a3), turn 2 = 101+101, turn 1 = 101+…
  assert.equal(cutPoint(branch(), 40, estimateTokens).id, 'x3');
  assert.equal(cutPoint(branch(), 240, estimateTokens).id, 'u2', 'two turns fit; never the tool result or its call');
  assert.equal(cutPoint(branch(), 10, estimateTokens), null, 'not even the newest turn');
  // Inside the context only: entries before the newest compaction's kept entry are already out.
  const compacted = [...branch().slice(0, 4), { type: 'compaction', id: 'c0', firstKeptEntryId: 'u2', summary: 'S' }, ...branch().slice(4)];
  assert.equal(lastCompaction(compacted), 'c0');
  assert.equal(cutPoint(compacted, 10_000, estimateTokens).id, 'u2');
  const r = rig();
  const path = join(dir, 'cut.json');
  // Nothing beyond the messages in Pi's count, a short summary: two turns fit.
  const bare = { ...preparation, previousSummary: '## Goal\n- earlier', tokensBefore: visibleTokens(branch(), estimateTokens) + 5 };
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 320, tokensBefore: 334007, at: '28 Oct 17:28' }));
  await r.command(SUPPLY_COMMAND, path);
  const out = await r.hook({ preparation: bare, branchEntries: branch() });
  assert.equal(out.compaction.firstKeptEntryId, 'u2');
  assert.equal(out.compaction.details.fixedTokens, 0);
  assert.ok(out.compaction.summary.startsWith('## Goal\n- earlier\n\n[Heimdall, 28 Oct 17:28: the older part'));
  assert.ok(out.compaction.summary.includes('334007 tokens'));
  assert.equal(out.compaction.details.heimdall, 'cut');
  // A turn bigger than the budget: Pi's own cut (its recent tokens), never nothing.
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 5, tokensBefore: 1, at: 'now' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.equal((await r.hook({ preparation, branchEntries: branch() })).compaction.firstKeptEntryId, 'u3');
});

test('after the cut the whole context, summary and Pi\'s fixed part included, sits under the compaction mark', async () => {
  // Review 28 Oct: a 500-token budget (mark 425 at 85%), the cut asked for 80% (400). Pi's count holds 100 tokens beyond
  // the messages (system prompt, tools) and a 100-token previous summary. Counting only the messages kept u2's two turns
  // (234) and left 500 tokens after the cut, past the mark: compaction again at once, and a second notice.
  const r = rig();
  const previousSummary = 'S'.repeat(400);
  const p = { ...preparation, previousSummary, tokensBefore: visibleTokens(branch(), estimateTokens) + 100 + 100 };
  const path = join(dir, 'cut-room.json');
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 400, tokensBefore: 334007, sourceKept: 'u3', previous: null, at: '28 Oct 17:28' }));
  await r.command(SUPPLY_COMMAND, path);
  const out = (await r.hook({ preparation: p, branchEntries: branch() })).compaction;
  const after = 100 + Math.ceil(out.summary.length / 4) + out.details.keptTokens;
  assert.equal(out.details.fixedTokens, 100);
  assert.equal(out.details.afterTokens, after);
  assert.ok(after <= 400 && after < 425, `after the cut: ${after}`);
  assert.equal(out.firstKeptEntryId, 'x3');
});

test('a cut goes only to the history it was made for, never a new or rotated session', async () => {
  const r = rig();
  const path = join(dir, 'cut-origin.json');
  // Made for a history whose kept entry was 'old-k'; this session (no summary either) never had it.
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 400, tokensBefore: 1, sourceKept: 'old-k', previous: null, at: 'now' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.deepEqual(await r.hook({ preparation, branchEntries: branch() }), { cancel: true });
  writeFileSync(path, JSON.stringify({ mode: 'cut', keepTokens: 400, tokensBefore: 1, sourceKept: 'u3', previous: null, at: 'now' }));
  await r.command(SUPPLY_COMMAND, path);
  assert.equal((await r.hook({ preparation, branchEntries: branch() })).compaction.details.heimdall, 'cut');
});
