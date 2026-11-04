// Local times are the user's (Poland; 26 Oct is after the switch to CET, UTC+1).
process.env.TZ = 'Europe/Warsaw';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { headerLimits, bodyLimits, isUsageLimit, limitLine, createQuotaWatch, quotaFetch, registerQuota } from './quota.mjs';

// 26 Oct 23:33:14 CET, the first failed wake; the five-hour window reset at 03:04:08 (Codex's rate_limits,
// primary resets_at 1793066648).
const T = Date.parse('2026-10-26T22:33:14Z');
const RESET = 1793066648;
const MESSAGE = 'The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.';
const URL = 'https://api.openai.com/v1/responses';

function sse(events) {
  const text = events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join('');
  const bytes = new TextEncoder().encode(text);
  return new ReadableStream({ start(c) { c.enqueue(bytes.slice(0, 40)); c.enqueue(bytes.slice(40)); c.close(); } });
}
function rig(clock = { t: T }) {
  const frames = [];
  const watch = createQuotaWatch({ provider: 'openai', emit: (f) => frames.push(f), now: () => clock.t });
  return { frames, watch, clock };
}
/** One request through quotaFetch and its consumer, ending like Pi's message_end. */
async function request(watch, response, message) {
  const fetch = quotaFetch(async () => response, watch);
  const r = await fetch(URL, { method: 'POST', body: '{}' });
  if (r.body) await r.text(); // the SDK reads the original
  await watch.messageEnd(message);
}
const failed = (errorMessage = MESSAGE) => ({ role: 'assistant', stopReason: 'error', errorMessage });
const ok = { role: 'assistant', stopReason: 'stop' };

test('tonight: an SSE usage-limit error after a 200, the reset from the codex headers', async () => {
  const { frames, watch } = rig();
  const headers = { 'content-type': 'text/event-stream', 'x-codex-primary-used-percent': '100', 'x-codex-primary-window-minutes': '300',
    'x-codex-primary-reset-at': String(RESET), 'x-codex-secondary-used-percent': '13', 'x-codex-secondary-window-minutes': '10080',
    'x-codex-secondary-reset-at': '1793653448' };
  const body = sse([['response.created', { type: 'response.created', response: { id: 'resp_1' } }],
    ['error', { type: 'error', code: 'subscription_sharing_usage_limit_exceeded', message: MESSAGE }]]);
  await request(watch, new Response(body, { status: 200, headers }), failed());
  assert.equal(frames.length, 1);
  assert.equal(frames[0].line, "ChatGPT's five-hour limit is used up; it resets at 3:04.");
  assert.equal(frames[0].source, 'header');
  assert.equal(frames[0].code, 'subscription_sharing_usage_limit_exceeded');
  assert.equal(frames[0].resets_at_ms, RESET * 1000);
  assert.equal(frames[0].repeat, false);
});

test('the reset time stated in an error body wins and is named by its matching window', async () => {
  const { frames, watch } = rig();
  const body = JSON.stringify({ error: { type: 'usage_limit_reached', message: 'The usage limit has been reached', resets_at: RESET,
    rate_limits: { primary: { used_percent: 100, window_minutes: 300, resets_at: RESET } } } });
  await request(watch, new Response(body, { status: 429, headers: { 'content-type': 'application/json' } }), failed('429 The usage limit has been reached'));
  assert.equal(frames[0].line, "ChatGPT's five-hour limit is used up; it resets at 3:04.");
  assert.equal(frames[0].source, 'body');
  // A body with only seconds to go.
  assert.equal(bodyLimits('{"error":{"code":"x","resets_in_seconds":60}}', T).resetsAtMs, T + 60_000);
});

test('no reset time anywhere: the line has no time, never a guessed one', async () => {
  const { frames, watch } = rig();
  // A window at 85% is not proof of which limit ran out, so its reset is not used.
  const headers = { 'content-type': 'application/json', 'x-codex-primary-used-percent': '85', 'x-codex-primary-reset-at': String(RESET) };
  await request(watch, new Response(JSON.stringify({ error: { code: 'subscription_sharing_usage_limit_exceeded', message: MESSAGE } }), { status: 429, headers }), failed());
  assert.equal(frames[0].line, "ChatGPT's usage limit is used up.");
  assert.equal(frames[0].resets_at_ms, null);
  assert.equal(frames[0].source, 'none');
  assert.doesNotMatch(frames[0].line, /\d/);
});

test('a 429 retry-after is a stated reset; a weekly reset names the day', () => {
  const h = headerLimits(new Headers({ 'retry-after': '120' }), T);
  assert.equal(h.retryAfterMs, 120_000);
  assert.equal(limitLine('ChatGPT', 10080, 1793653448 * 1000, T), "ChatGPT's weekly limit is used up; it resets on Monday at 22:04.");
  assert.equal(limitLine('ChatGPT', 300, undefined, T), "ChatGPT's five-hour limit is used up.");
});

test('once per limit window: later failures are logged as repeats, the next window speaks again', async () => {
  const { frames, watch, clock } = rig();
  const headers = { 'content-type': 'application/json', 'x-codex-primary-used-percent': '100', 'x-codex-primary-window-minutes': '300', 'x-codex-primary-reset-at': String(RESET) };
  const limited = () => new Response(JSON.stringify({ error: { code: 'subscription_sharing_usage_limit_exceeded' } }), { status: 429, headers });
  await request(watch, limited(), failed());
  clock.t += 60_000;
  await request(watch, limited(), failed());
  clock.t += 60_000;
  await request(watch, limited(), failed());
  assert.deepEqual(frames.map((f) => f.repeat), [false, true, true]);
  assert.ok(frames.every((f) => f.key === frames[0].key), 'one identity for the window');
  // After the reset the limit is a new one.
  clock.t = RESET * 1000 + 5 * 3600_000;
  const next = RESET + 5 * 3600 + 3600;
  await request(watch, new Response('{"error":{"code":"usage_limit_reached"}}', { status: 429, headers: { ...headers, 'x-codex-primary-reset-at': String(next) } }), failed('usage_limit_reached'));
  assert.equal(frames.at(-1).repeat, false);
  assert.notEqual(frames.at(-1).key, frames[0].key);
});

test('without a reset time, one line until a success shows the limit is gone', async () => {
  const { frames, watch, clock } = rig();
  const limited = () => new Response('{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}', { status: 429 });
  await request(watch, limited(), failed());
  clock.t += 3600_000;
  await request(watch, limited(), failed());
  await request(watch, new Response('{}', { status: 200 }), ok);
  clock.t += 60_000;
  await request(watch, limited(), failed());
  assert.deepEqual(frames.map((f) => f.repeat), [false, true, false]);
  assert.notEqual(frames[0].key, frames[2].key);
});

test('other failures say nothing: a throttle, a timeout, an abort', async () => {
  const { frames, watch } = rig();
  for (const e of ['429 Rate limit reached for requests', 'Request timed out.', 'OpenAI API error (500): server_error']) {
    await request(watch, new Response('{"error":{"code":"rate_limit_exceeded"}}', { status: 429 }), failed(e));
  }
  await watch.messageEnd({ role: 'assistant', stopReason: 'aborted', errorMessage: MESSAGE });
  assert.deepEqual(frames, []);
  // An earlier limit's body never colours a later failure that got no response at all.
  await request(watch, new Response('{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}', { status: 429 }), failed());
  const offline = quotaFetch(async () => { throw new Error('fetch failed'); }, watch);
  await assert.rejects(offline(URL, { method: 'POST' }));
  await watch.messageEnd(failed('Connection error.'));
  assert.equal(frames.length, 1, 'only the real limit');
  assert.ok(isUsageLimit(MESSAGE) && isUsageLimit('subscription_sharing_usage_limit_exceeded') && isUsageLimit('insufficient_quota'));
  assert.ok(!isUsageLimit('429 Rate limit reached') && !isUsageLimit('overloaded'));
});

test('the five-hour window past 80% warns once per window, after a successful reply', async () => {
  const { frames, watch, clock } = rig();
  const at = (used, reset = RESET) => new Response('{}', { status: 200, headers: { 'x-codex-primary-used-percent': String(used), 'x-codex-primary-window-minutes': '300', 'x-codex-primary-reset-at': String(reset) } });
  await request(watch, at(79), ok);
  assert.deepEqual(frames, [], 'below 80%');
  await request(watch, at(81), ok);
  clock.t += 60_000;
  await request(watch, at(86), ok);
  await request(watch, at(95), ok);
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, 'warning');
  assert.equal(frames[0].line, "ChatGPT's five-hour limit is 81% used; it resets at 3:04.");
  // The next window warns again.
  clock.t = RESET * 1000 + 60_000;
  await request(watch, at(82, RESET + 5 * 3600), ok);
  assert.equal(frames.length, 2);
  assert.equal(frames[1].line, "ChatGPT's five-hour limit is 82% used; it resets at 8:04.");
  // The weekly window is not warned about.
  const { frames: weekly, watch: w2 } = rig();
  await request(w2, new Response('{}', { status: 200, headers: { 'x-codex-secondary-used-percent': '90', 'x-codex-secondary-window-minutes': '10080' } }), ok);
  assert.deepEqual(weekly, []);
});

test('only OpenAI routes are watched, and the response reaches the caller untouched', async () => {
  const saved = globalThis.fetch;
  try {
    const none = {};
    assert.equal(registerQuota({ on: (n, f) => { none[n] = f; } }, 'anthropic', () => {}), undefined);
    assert.deepEqual(none, {});
    const seen = [];
    const watch = createQuotaWatch({ provider: 'openai', emit: () => {}, now: () => T });
    const original = watch.response;
    watch.response = (s, h) => { seen.push(s); original(s, h); };
    const fetch = quotaFetch(async () => new Response('hello', { status: 200 }), watch);
    assert.equal(await (await fetch('https://api.anthropic.com/v1/messages', { method: 'POST' })).text(), 'hello');
    assert.equal(await (await fetch(URL, { method: 'POST' })).text(), 'hello');
    assert.deepEqual(seen, [200], 'only the OpenAI request was observed');
    const hooks = {};
    globalThis.fetch = async () => new Response('x');
    assert.ok(registerQuota({ on: (n, f) => { hooks[n] = f; } }, 'openai', () => {}));
    assert.deepEqual(Object.keys(hooks).sort(), ['message_end', 'provider_stream_event']);
  } finally {
    globalThis.fetch = saved;
  }
});
