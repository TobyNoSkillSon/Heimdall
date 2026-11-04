import test from 'node:test';
import assert from 'node:assert/strict';
import { thinkingOff, OFF_EFFORT } from './thinking-policy.mjs';

// What Pi 1.4.0 sends for a Claude 5.5 model at thinking Off (pi-ai anthropic-messages.js buildParams: `disabled`, no
// effort, no per-message effort messages). The effort-only message here stands for any Pi might add later.
const off = (model = 'claude-sonnet-5-5') => ({
  model, max_tokens: 16000, stream: true,
  thinking: { type: 'disabled' },
  messages: [
    { role: 'user', content: [{ type: 'text', text: 'hi' }] },
    { role: 'assistant', content: [{ type: 'thinking', thinking: '', signature: 'sig' }, { type: 'text', text: 'hello' }] },
    { role: 'user', content: [{ type: 'text', text: 'again' }] },
    { role: 'system', content: [{ type: 'text', text: 'an operator note stays' }] },
    { role: 'system', content: [], output_config: { effort: 'high' } },
  ],
});
// What Pi 1.4.0 sends at any level above Off for a managed-effort model.
const managed = () => ({
  ...off(), thinking: { type: 'adaptive', display: 'summarized', block_binding: { prefix_mismatch_behavior: 'drop_block' } },
  output_config: { effort: 'high' },
});

test('Sonnet Off: between_tools alone, one top-level effort at Low, no per-message effort', () => {
  const out = thinkingOff(off(), 'between_tools');
  assert.deepEqual(out.thinking, { type: 'between_tools' });
  assert.deepEqual(out.output_config, { effort: OFF_EFFORT });
  assert.equal(OFF_EFFORT, 'low');
  assert.equal(out.messages.length, 4);
  assert.ok(out.messages.every(m => m.output_config === undefined), 'no effort-only messages');
  assert.equal(out.messages[3].content[0].text, 'an operator note stays');
  assert.equal(out.messages[1].content[0].type, 'thinking', 'earlier thinking blocks are passed back unchanged');
});

test('Haiku Off: thinking stays disabled and gains effort Low', () => {
  const out = thinkingOff(off('claude-haiku-5-5'), 'disabled');
  assert.deepEqual(out.thinking, { type: 'disabled' });
  assert.deepEqual(out.output_config, { effort: 'low' });
});

test('levels above Off, other modes and other providers pass through untouched', () => {
  const p = managed();
  assert.equal(thinkingOff(p, 'between_tools'), p, 'adaptive means a level above Off');
  const o = off();
  assert.equal(thinkingOff(o, undefined), o);
  assert.equal(thinkingOff(o, 'adaptive'), o);
  const openai = { model: 'kimi-k3', messages: [], reasoning_effort: 'max' };
  assert.equal(thinkingOff(openai, 'between_tools'), openai);
});
