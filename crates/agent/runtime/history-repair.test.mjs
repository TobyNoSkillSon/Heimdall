import test from 'node:test';
import assert from 'node:assert/strict';
import { historyRejection, imageInfo, imageFix, imageRepairs, failedAttempt, rebuiltSummary, requestMessage, pendingRequests, userLine, registerHistoryRepair, MAX_IMAGE_BASE64, REBUILT_CHARS, REQUEST_TYPE } from './history-repair.mjs';

const png = (w, h) => { const b = Buffer.alloc(33); Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52]).copy(b); b.writeUInt32BE(w, 16); b.writeUInt32BE(h, 20); return b; };
const jpeg = (w, h) => Buffer.from([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, ...Buffer.from('JFIF\0\x01\x01\0\0\x01\0\x01\0\0', 'latin1'),
  0xff, 0xdb, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, h >> 8, h & 255, w >> 8, w & 255, 0x03, 0x01, 0x22, 0x00]);
const image = (bytes, mimeType) => ({ type: 'image', data: bytes.toString('base64'), mimeType });
// The 20 Oct 19:20:55 error, verbatim.
const REFUSED = '400 {"type":"error","error":{"type":"invalid_request_error","message":"messages.345.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image"},"request_id":"req_011CgDsdBwYLfQF5VvUVEJQF"}';

// Pi's boundary context (`contextEntries`): the 20 Oct history after its compaction, shortened.
const entries = () => [
  { sourceEntry: { type: 'compaction', id: 'c', summary: '## Goal\n- User uses Heimdall as a voice orchestrator.' }, messages: [{ role: 'system', content: '' }, { role: 'compactionSummary', summary: '## Goal' }] },
  { sourceEntry: { type: 'message', id: 'kept' }, messages: [{ role: 'user', content: [{ type: 'text', text: '[Heimdall delivery: d1]\nTranscript:\n[19:20:35 Mic · User] show me YouTube' }, image(png(1728, 1117), 'image/png')] }] },
  { sourceEntry: { type: 'message', id: 'call' }, messages: [{ role: 'assistant', stopReason: 'toolUse', content: [{ type: 'text', text: 'Opening it.' }, { type: 'toolCall', id: 't1', name: 'mcp__heimdall__look', arguments: {} }] }] },
  { sourceEntry: { type: 'message', id: 'shot' }, messages: [{ role: 'toolResult', toolName: 'mcp__heimdall__look', content: [image(jpeg(1728, 1117), 'image/png'), { type: 'text', text: 'Display 1: size 1728 1117' }] }] },
  { sourceEntry: { type: 'custom', id: 'fx' }, messages: [] },
  { sourceEntry: { type: 'message', id: 'err' }, messages: [{ role: 'assistant', stopReason: 'error', content: [], errorMessage: REFUSED }] },
];

test('only a 400 that names a block of the history is a history rejection', () => {
  assert.ok(historyRejection(REFUSED));
  assert.ok(historyRejection('400 messages.118.content.0.tool_result.content.0.image.source.base64: …'));
  assert.ok(historyRejection("400 Invalid 'messages[12].content[1].image_url': unsupported image"));
  assert.ok(!historyRejection('400 invalid_request_error: messages.5.content.0: Invalid `signature` in `thinking` block'), 'provider-retry resends those');
  assert.ok(!historyRejection('400 {"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}'));
  assert.ok(!historyRejection('400 invalid_request_error: Your credit balance is too low to access the Anthropic API.'));
  assert.ok(!historyRejection('529 overloaded_error'));
  assert.ok(!historyRejection('fetch failed'));
  assert.ok(!historyRejection(undefined));
});

test('formats and sizes come from the bytes', () => {
  assert.deepEqual(imageInfo(png(1728, 1117)), ['image/png', 1728, 1117]);
  assert.deepEqual(imageInfo(jpeg(1728, 1117)), ['image/jpeg', 1728, 1117]);
  assert.deepEqual(imageInfo(Buffer.from('GIF89a\x40\x01\xc8\x00', 'latin1')), ['image/gif', 320, 200]);
  assert.equal(imageInfo(Buffer.from('%PDF-1.7')), null);
  assert.deepEqual(imageInfo(Buffer.from('/9j/4AAQSkZJRgABAQAAAQABAAD/', 'base64')), ['image/jpeg', 0, 0], 'the real 20 Oct screenshot header');
});

test('each documented image rule has its fix', () => {
  assert.equal(imageFix(image(png(1728, 1117), 'image/png'), false), null);
  assert.deepEqual(imageFix(image(jpeg(1728, 1117), 'image/png'), false), { relabel: 'image/jpeg', from: 'image/png' });
  assert.deepEqual(imageFix(image(png(8001, 10), 'image/png'), false), { remove: 'wider or taller than 8000 pixels' });
  assert.equal(imageFix(image(png(2001, 10), 'image/png'), false), null);
  assert.ok(imageFix(image(png(2001, 10), 'image/png'), true).remove);
  assert.deepEqual(imageFix({ type: 'image', data: 'not base64!', mimeType: 'image/png' }, false), { remove: 'its data was unreadable' });
  assert.deepEqual(imageFix(image(Buffer.from('<html>'), 'image/png'), false), { remove: 'not a PNG, JPEG, GIF or WebP image' });
  assert.deepEqual(imageFix({ type: 'image', data: 'A'.repeat(MAX_IMAGE_BASE64 + 4), mimeType: 'image/png' }, false), { remove: 'larger than the model accepts' });
});

test('the 20 Oct history: one edit relabels the screenshot and keeps everything else', () => {
  const { drafts, relabelled, removed } = imageRepairs(entries());
  assert.deepEqual(relabelled, ['image/png → image/jpeg']);
  assert.deepEqual(removed, []);
  assert.equal(drafts.length, 1);
  assert.equal(drafts[0].type, 'context_edit');
  assert.equal(drafts[0].targetId, 'shot');
  assert.equal(drafts[0].replacement.content[0].mimeType, 'image/jpeg');
  assert.equal(drafts[0].replacement.content[0].data, entries()[3].messages[0].content[0].data, 'the image itself is kept');
  assert.deepEqual(drafts[0].replacement.content[1], { type: 'text', text: 'Display 1: size 1728 1117' });
  assert.equal(failedAttempt(entries()), 'err');
  assert.equal(failedAttempt([...entries(), { sourceEntry: { type: 'usage', id: 'u' }, messages: [] }]), 'err', 'bookkeeping after the attempt');
  assert.equal(failedAttempt(entries().slice(0, 4)), null);
});

test('more than twenty images hold every image to 2000 pixels', () => {
  const many = Array.from({ length: 21 }, (_, n) => ({ sourceEntry: { type: 'message', id: `u${n}` }, messages: [{ role: 'user', content: [image(png(n === 3 ? 2576 : 1000, 600), 'image/png')] }] }));
  const { drafts, removed } = imageRepairs(many);
  assert.deepEqual(drafts.map((d) => d.targetId), ['u3']);
  assert.equal(drafts[0].replacement.content[0].type, 'text');
  assert.equal(removed.length, 1);
  assert.equal(imageRepairs(many.slice(0, 20)).drafts.length, 0);
});

test('the rebuilt summary keeps the last summary and the conversation since as text, newest kept', () => {
  const summary = rebuiltSummary(entries());
  assert.ok(summary.startsWith('## Goal\n- User uses Heimdall'));
  // The request being answered comes once, after the summary (`requestMessage`); here only where it came in.
  assert.ok(!summary.includes('show me YouTube'), summary);
  assert.ok(summary.includes("User (or Heimdall's wake): (the request answered now; its words follow this summary)\nHeimdall: Opening it."), summary);
  assert.ok(summary.includes('the request answered now follows this summary, and nothing else here is a request'), summary);
  assert.ok(!summary.includes('nothing here is a new request'), summary);
  assert.ok(!summary.includes('[Heimdall delivery:'));
  assert.ok(summary.includes('Heimdall: Opening it.'));
  assert.ok(summary.includes('Heimdall called mcp__heimdall__look'));
  assert.ok(summary.includes('Result of mcp__heimdall__look: Display 1: size 1728 1117'));
  assert.ok(!summary.includes('invalid_request_error'), 'the failed attempt is not history');
  assert.ok(!summary.includes(entries()[3].messages[0].content[0].data.slice(0, 40)), 'no image data');
  // A long conversation of answered requests (each user line followed by a completed reply).
  const long = [...entries().slice(0, 1), ...Array.from({ length: 4000 }, (_, n) => ({ sourceEntry: { type: 'message', id: `m${n}` }, messages: [{ role: 'user', content: `line ${n} ${'x'.repeat(60)}` }, { role: 'assistant', stopReason: 'stop', content: [{ type: 'text', text: 'ok' }] }] }))];
  const bounded = rebuiltSummary(long);
  assert.ok(bounded.length < REBUILT_CHARS + 1000);
  assert.ok(bounded.includes('line 3999 ') && !bounded.includes('line 0 '));
});

test('a rebuilt summary line drops the marker and turns the date line into the time it was said', () => {
  assert.equal(userLine('[Heimdall delivery: d1]\n[Now: Tuesday 20 Oct 2026, 19:20]\nTranscript:\n[19:20:35 Mic · User] show me YouTube'),
    "User (or Heimdall's wake), Tuesday 20 Oct 2026, 19:20: Transcript:\n[19:20:35 Mic · User] show me YouTube");
  assert.equal(userLine('[Heimdall delivery: d1]\nhi'), "User (or Heimdall's wake): hi", 'deliveries from before the date line');
  assert.equal(userLine('[Now: what\nis this]\nx'), "User (or Heimdall's wake): [Now: what\nis this]\nx", 'only a whole stamp line');
  assert.equal(userLine('plain words'), "User (or Heimdall's wake): plain words");
});

// Night review 21 Oct (MUST 2): after a rebuild the model read only a summary that said "nothing here is a new
// request", answered NO_REPLY, the delivery settled and the user's words were lost. The request now follows the summary.
test('the request after a rebuild is every user message since the last completed reply, in its own words', () => {
  const at = (id, m) => ({ sourceEntry: { type: 'message', id }, messages: [m] });
  const context = [
    entries()[0],
    at('old', { role: 'user', content: [{ type: 'text', text: '[Heimdall delivery: d0]\n[Now: Tuesday 20 Oct 2026, 19:10]\nan answered question' }] }),
    at('reply', { role: 'assistant', stopReason: 'stop', content: [{ type: 'text', text: 'Answered.' }] }),
    at('ask', { role: 'user', content: [{ type: 'text', text: '[Heimdall delivery: d1]\n[Now: Tuesday 20 Oct 2026, 19:20]\nTranscript:\n[19:20:35 Mic · User] show me YouTube' }, image(png(10, 10), 'image/png')] }),
    entries()[2], entries()[3],
    at('steer', { role: 'user', content: '[Heimdall delivery: d2]\n[Now: Tuesday 20 Oct 2026, 19:20]\nTranscript:\n[19:20:50 Mic · User] and make it full screen' }),
    entries()[5],
  ];
  assert.deepEqual(pendingRequests(context).map((m) => m.content[0]?.text ?? m.content).map((t) => t.split('\n')[0]), ['[Heimdall delivery: d1]', '[Heimdall delivery: d2]']);
  const request = requestMessage(context);
  assert.equal(request.type, 'custom_message');
  assert.equal(request.customType, REQUEST_TYPE);
  assert.equal(request.display, false, 'the Agent window already shows his words');
  assert.ok(request.content.startsWith('The request you are answering now.'), request.content);
  assert.ok(request.content.includes('[Now: Tuesday 20 Oct 2026, 19:20]\nTranscript:\n[19:20:35 Mic · User] show me YouTube'), request.content);
  assert.ok(request.content.includes('[19:20:50 Mic · User] and make it full screen'), 'a steer is part of the request');
  assert.ok(!request.content.includes('an answered question'), 'an answered request stays history');
  assert.ok(!request.content.includes('[Heimdall delivery:'), 'no delivery marker');
  assert.ok(request.content.endsWith('(1 image sent with it is left out.)'), request.content);
  assert.ok(!request.content.includes(image(png(10, 10), 'image/png').data), 'no image data');
  const summary = rebuiltSummary(context);
  assert.ok(summary.includes("User (or Heimdall's wake), Tuesday 20 Oct 2026, 19:10: an answered question\nHeimdall: Answered."), summary);
  assert.ok(!summary.includes('show me YouTube') && !summary.includes('full screen'), 'his request comes once, after the summary');
  // A request a previous rebuild carried, still unanswered, is carried again (without the old framing).
  const carried = [{ sourceEntry: { type: 'compaction', id: 'c2', summary: 's' }, messages: [] },
    { sourceEntry: { type: 'custom_message', id: 'q' }, messages: [{ role: 'custom', customType: REQUEST_TYPE, content: request.content }] }, entries()[5]];
  const again = requestMessage(carried).content;
  assert.equal(again.split('The request you are answering now.').length, 2, 'one framing paragraph');
  assert.ok(again.includes('show me YouTube') && again.includes('make it full screen'), again);
  // A request the user stopped (its turn aborted) is not revived; only what came after it is the request.
  const stopped = [entries()[0], at('s', { role: 'user', content: '[Heimdall delivery: d0]\ndelete the old builds' }),
    at('ab', { role: 'assistant', stopReason: 'aborted', content: [] }), at('n', { role: 'user', content: '[Heimdall delivery: d1]\nwhat time is it' }), entries()[5]];
  assert.deepEqual(pendingRequests(stopped).map((m) => m.content), ['[Heimdall delivery: d1]\nwhat time is it']);
  assert.ok(!requestMessage(stopped).content.includes('delete the old builds'));
  // Nothing asked since the last reply: no request message, and the summary says so.
  const answered = [entries()[0], at('old', { role: 'user', content: 'x' }), at('reply', { role: 'assistant', stopReason: 'stop', content: [] }), entries()[5]];
  assert.equal(requestMessage(answered), null);
  assert.ok(rebuiltSummary(answered).includes('nothing here is a new request'));
});

/** Pi's model view after the boundary's drafts, as Pi builds it (`buildContextEntries` after a compaction that keeps
 * nothing, `convertToLlm`): the summary, then every message entry appended after it; custom entries carry none. */
const lastModelMessage = (drafts) => {
  const at = drafts.findLastIndex((d) => d.type === 'compaction');
  const visible = drafts.slice(at).flatMap((d) => (d.type === 'compaction' ? [{ role: 'user', text: d.summary }] : d.type === 'custom_message' ? [{ role: 'user', text: d.content }] : []));
  return visible.at(-1);
};
const stateOf = (r) => r.entries.find((d) => d.customType === 'heimdall_history_repair')?.data;

/** A Pi stand-in that keeps the handlers and runs `agent_before_settle` like Pi's boundary: in order, chaining. */
function fakePi() {
  const handlers = {};
  const pi = { on: (name, f) => { (handlers[name] ??= []).push(f); } };
  const settle = async (contextEntries, outcome = 'error', start = { entries: [], continue: false }) => {
    let state = start;
    for (const f of handlers.agent_before_settle ?? []) {
      const r = await f({ type: 'agent_before_settle', outcome, ...state, context: { contextEntries } });
      if (r?.entries !== undefined) state = { ...state, entries: r.entries };
      if (r?.continue !== undefined) state = { ...state, continue: r.continue };
    }
    return state;
  };
  return { pi, settle, emit: (name) => handlers[name]?.forEach((f) => f({})) };
}

test('a refused history is repaired and continued once, then rebuilt from its summary, then left to fail', async () => {
  const { pi, settle, emit } = fakePi();
  registerHistoryRepair(pi);
  const first = await settle(entries());
  assert.equal(first.continue, true);
  assert.deepEqual(first.entries.map((d) => [d.type, d.targetId, d.replacement === null]), [['context_edit', 'shot', false], ['context_edit', 'err', true], ['custom', undefined, false]]);
  // The line is a session entry Pi announces itself (`entry_appended`): the Agent window, log and island read it.
  assert.deepEqual(first.entries[2], { type: 'custom', customType: 'heimdall_history_repair', data: { state: 'repaired',
    line: 'The model refused the conversation; repaired it: 1 image relabelled (image/png → image/jpeg). Answering again.' } });
  // Refused again after the repair: the history restarts from the summary, and the request is the last message.
  const again = [...entries().slice(0, 5), { sourceEntry: { type: 'message', id: 'err2' }, messages: [{ role: 'assistant', stopReason: 'error', content: [], errorMessage: '400 messages.7.content.2: something else' }] }];
  const second = await settle(again);
  assert.equal(second.continue, true);
  assert.deepEqual(second.entries.map((d) => d.type), ['compaction', 'custom_message', 'custom']);
  assert.equal(second.entries[0].firstKeptEntryId, undefined, 'keeps nothing it cannot say in words');
  assert.ok(second.entries[0].summary.startsWith('## Goal'));
  assert.equal(stateOf(second).state, 'restarted_from_summary');
  const last = lastModelMessage(second.entries);
  assert.equal(last.role, 'user');
  assert.ok(last.text.startsWith('The request you are answering now.') && last.text.includes('[19:20:35 Mic · User] show me YouTube'), last.text);
  // A third refusal settles with the error; Heimdall says the conversation is broken.
  const third = await settle(again);
  assert.equal(third.continue, false);
  assert.equal(stateOf(third).state, 'gave_up');
  assert.deepEqual(third.entries.map((d) => d.data?.state), ['gave_up'], 'only the line, no edit');
  // Settlement starts over.
  emit('agent_settled');
  assert.equal((await settle(entries())).continue, true);
});

test('nothing repairable goes straight to the summary; other failures and other continuations are left alone', async () => {
  const { pi, settle } = fakePi();
  registerHistoryRepair(pi);
  const clean = entries().map((e) => (e.sourceEntry.id === 'shot' ? { ...e, messages: [{ role: 'toolResult', content: [{ type: 'text', text: 'ok' }] }] } : e));
  const r = await settle(clean);
  assert.equal(r.continue, true);
  assert.equal(r.entries[0].type, 'compaction');
  assert.equal(stateOf(r).state, 'restarted_from_summary');
  assert.ok(lastModelMessage(r.entries).text.includes('show me YouTube'), 'the model answers the request, not NO_REPLY to history');
  for (const error of ['529 overloaded_error', '400 invalid_request_error: messages.5.content.0: Invalid `signature` in `thinking` block']) {
    const { pi: p2, settle: s2 } = fakePi();
    registerHistoryRepair(p2);
    const other = [...entries().slice(0, 5), { sourceEntry: { type: 'message', id: 'e' }, messages: [{ role: 'assistant', stopReason: 'error', content: [], errorMessage: error }] }];
    const left = await s2(other);
    assert.equal(left.continue, false, error);
    assert.deepEqual(left.entries, [], 'nothing written');
  }
  const { pi: p3, settle: s3 } = fakePi();
  registerHistoryRepair(p3);
  assert.equal((await s3(entries(), 'completed')).continue, false, 'a completed run is not touched');
  const continued = await s3(entries(), 'error', { entries: [{ type: 'custom', customType: 'x', data: {} }], continue: true });
  assert.deepEqual(continued.entries, [{ type: 'custom', customType: 'x', data: {} }], 'another handler already continues the run');
});

test('the public edition names the speaker User, never the owner (VM finding P-04)', () => {
  const before = process.env.HEIMDALL_EDITION;
  process.env.HEIMDALL_EDITION = 'public';
  try {
    assert.equal(userLine('[Heimdall delivery: d1]\nhi'), "User (or Heimdall's wake): hi");
  } finally {
    if (before === undefined) delete process.env.HEIMDALL_EDITION; else process.env.HEIMDALL_EDITION = before;
  }
  assert.equal(userLine('[Heimdall delivery: d1]\nhi'), "User (or Heimdall's wake): hi");
});
