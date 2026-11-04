import { test } from 'node:test';
import assert from 'node:assert/strict';
import { normalized, policy, registerLoopGuard } from '../../runtime/loop-guard.mjs';
function fixture() {
  const handlers = new Map(), entries = [];
  registerLoopGuard({ on: (type, fn) => handlers.set(type, fn), appendEntry: (type, data) => entries.push({ type, data }) });
  let id = 0;
  const emit = event => handlers.get(event.type)?.(event);
  return { emit, entries, call(toolName = 'read', input = { path: 'a' }, result = { content: [{ type: 'text', text: 'same' }] }, isError = false) {
    const toolCallId = String(++id);
    const decision = emit({ type: 'tool_call', toolCallId, toolName, input });
    // Pi emits an execution-end event even for a blocked call.
    emit({ type: 'tool_execution_end', toolCallId, toolName, isError: decision ? true : isError,
      result: decision ? { content: [{ type: 'text', text: decision.reason }] } : result });
    return decision;
  } };
}
test('five identical pairs execute, sixth blocks, two more produce the abort count', () => {
  assert.equal(policy.identicalResults, 5);
  assert.equal(policy.blockedAttempts, 3);
  for (const tool of ['read', 'bash', 'mcp__heimdall__transcript_search']) {
    const f = fixture();
    for (let i = 0; i < 5; i++) assert.equal(f.call(tool), undefined);
    for (let i = 1; i <= 3; i++) {
      const blocked = f.call(tool);
      assert.equal(blocked.block, true);
      assert.equal(blocked.reason, `You have called ${tool} with the same arguments 5 times and got the same result. Repeating will not change it; use the result, try something different, or finish and tell User.`);
      assert.equal(f.entries.at(-1).data.blocked_attempts, i);
      assert.equal(f.entries.at(-1).data.repeats, 5);
      assert.match(f.entries.at(-1).data.arguments_hash, /^[a-f0-9]{64}$/);
    }
  }
});
test('normalization sorts nested object keys, preserves array order, types and command whitespace', () => {
  assert.equal(normalized({ z: { b: 2, a: 1 }, a: [2, 1] }), normalized({ a: [2, 1], z: { a: 1, b: 2 } }));
  assert.notEqual(normalized({ a: [2, 1] }), normalized({ a: [1, 2] }));
  assert.notEqual(normalized({ command: 'echo x' }), normalized({ command: 'echo  x' }));
  assert.notEqual(normalized({ a: 1 }), normalized({ a: '1' }));
  const f = fixture();
  for (let i = 0; i < 5; i++) assert.equal(f.call('bash', i % 2 ? { b: 2, a: 1 } : { a: 1, b: 2 }), undefined);
  assert.equal(f.call('bash', { b: 2, a: 1 }).block, true);
});
test('changing polling results never form a loop', () => {
  for (const field of ['text', 'image', 'structuredContent', 'error']) {
    const f = fixture();
    for (let i = 0; i < 20; i++) {
      const result = { content: [{ type: 'text', text: field === 'text' ? String(i) : 'same' }] };
      if (field === 'image') result.content = [{ type: 'image', data: String(i), mimeType: 'image/png' }];
      if (field === 'structuredContent') result.structuredContent = { value: i };
      assert.equal(f.call('read', {}, result, field === 'error' && i % 2 === 0), undefined);
    }
    assert.equal(f.entries.length, 0);
  }
});
test('a changed result restarts the streak instead of accumulating matching calls', () => {
  const f = fixture();
  for (let i = 0; i < 4; i++) assert.equal(f.call(), undefined);
  assert.equal(f.call('read', { path: 'a' }, { content: [{ type: 'text', text: 'changed' }] }), undefined);
  for (let i = 0; i < 5; i++) assert.equal(f.call(), undefined);
  assert.equal(f.call().block, true);
});
test('another tool or exact command breaks adjacency', () => {
  for (const different of [f => f.call('bash'), f => f.call('read', { path: 'b' })]) {
    const f = fixture();
    for (let i = 0; i < 5; i++) f.call();
    assert.equal(different(f), undefined);
    for (let i = 0; i < 5; i++) assert.equal(f.call(), undefined);
    assert.equal(f.call().block, true);
  }
});
test('MCP action ids and execution metadata do not disguise identical output', () => {
  const f = fixture();
  for (let i = 0; i < 5; i++) assert.equal(f.call('mcp__heimdall__search', {}, {
    content: [{ type: 'text', text: 'same' }], details: { actionId: String(i), structuredContent: { found: 0 } },
  }), undefined);
  assert.equal(f.call('mcp__heimdall__search', {}).block, true);
});
test('overlapping/nested calls cannot reuse stale completions as consecutive evidence', () => {
  const f = fixture();
  for (let i = 0; i < 4; i++) f.call();
  for (const id of ['a', 'b']) f.emit({ type: 'tool_call', toolName: 'read', input: { path: 'a' }, toolCallId: id });
  for (const id of ['a', 'b']) f.emit({ type: 'tool_execution_end', toolName: 'read', toolCallId: id, isError: false, result: { content: [{ type: 'text', text: 'same' }] } });
  assert.equal(f.call(), undefined);
  assert.equal(f.entries.length, 0);
});
test('settlement, reload and Off reset both streak and blocked attempts; tool rounds do not', () => {
  for (const type of ['agent_settled', 'session_start', 'session_shutdown']) {
    const f = fixture();
    for (let i = 0; i < 5; i++) f.call();
    assert.equal(f.call().block, true);
    f.emit({ type });
    for (let i = 0; i < 5; i++) { f.emit({ type: 'turn_start' }); assert.equal(f.call(), undefined); }
    assert.equal(f.call().block, true);
    assert.equal(f.entries.at(-1).data.blocked_attempts, 1);
  }
});
test('explicit null structured output differs from absent output', () => {
  const f = fixture();
  for (let i = 0; i < 20; i++) {
    const result = { content: [{ type: 'text', text: 'same' }] };
    if (i % 2) result.structuredContent = null;
    assert.equal(f.call('read', {}, result), undefined);
  }
  assert.equal(f.entries.length, 0);
});
