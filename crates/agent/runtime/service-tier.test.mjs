import { test } from 'node:test';
import assert from 'node:assert/strict';
import { requestedTier, withTier, tierRecord, registerServiceTier } from './service-tier.mjs';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('fast asks OpenAI routes for priority; standard and other providers send no tier', () => {
  assert.equal(requestedTier('{"agent_speed":"fast"}', 'openai'), 'priority');
  assert.equal(requestedTier('{"agent_speed":"standard"}', 'openai'), undefined);
  assert.equal(requestedTier('{}', 'openai'), undefined);
  assert.equal(requestedTier('{"agent_speed":"fast"}', 'anthropic'), undefined);
  assert.equal(requestedTier('{"agent_speed":"fast"}', 'opencode-go'), undefined);
  assert.equal(requestedTier('not json', 'openai'), undefined);
});

test('only a Responses request gets the field', () => {
  assert.deepEqual(withTier({ model: 'gpt-6.1-sol', input: [] }, 'priority'), { model: 'gpt-6.1-sol', input: [], service_tier: 'priority' });
  const messages = { model: 'x', messages: [] };
  assert.equal(withTier(messages, 'priority'), messages);
  const plain = { model: 'x', input: [] };
  assert.equal(withTier(plain, undefined), plain);
});

test('requested and reported tiers are logged as they are, never as the effective mode', () => {
  const now = new Date('2026-10-26T22:00:00Z');
  assert.deepEqual(tierRecord({ type: 'response.completed', response: { id: 'r1', model: 'gpt-6.1-sol', service_tier: 'default' } }, 'priority', now),
    { at: '2026-10-26T22:00:00.000Z', event: 'response.completed', model: 'gpt-6.1-sol', response_id: 'r1', requested: 'priority', reported: 'default' });
  assert.equal(tierRecord({ type: 'response.created', response: {} }, undefined, now).requested, 'standard');
  assert.equal(tierRecord({ type: 'response.output_text.delta' }, 'priority', now), undefined);
});

test('the hooks read the setting per request and append one line per reporting event', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'tier-'));
  const settings = join(dir, 'settings.json'), log = join(dir, 'tier.jsonl');
  writeFileSync(settings, '{"agent_speed":"fast"}');
  const handlers = {};
  registerServiceTier({ on: (name, f) => { handlers[name] = f; } }, 'openai', settings, log);
  assert.equal(handlers.before_provider_request({ payload: { model: 'm', input: [] } }).service_tier, 'priority');
  handlers.provider_stream_event({ data: { type: 'response.completed', response: { id: 'r', service_tier: 'default' } } });
  writeFileSync(settings, '{"agent_speed":"standard"}');
  assert.equal(handlers.before_provider_request({ payload: { model: 'm', input: [] } }), undefined);
  const lines = readFileSync(log, 'utf8').trim().split('\n').map(JSON.parse);
  assert.deepEqual(lines.map(l => [l.requested, l.reported]), [['priority', 'default']]);
  const none = {};
  registerServiceTier({ on: (name, f) => { none[name] = f; } }, 'anthropic', settings, log);
  assert.deepEqual(none, {}, 'no hooks on other providers');
});
