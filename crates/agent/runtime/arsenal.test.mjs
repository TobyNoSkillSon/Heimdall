import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { registerArsenal, NAME } from './arsenal.mjs';

const BRIDGE = '/Applications/Arsenal.app/Contents/Helpers/arsenal-mcp';
function settings(value) {
  const dir = mkdtempSync(join(tmpdir(), 'arsenal-'));
  const file = join(dir, 'settings.json');
  writeFileSync(file, JSON.stringify(value));
  return file;
}
function pi() {
  const servers = [];
  return { servers, registerMcpServer: (name, config) => servers.push([name, config]) };
}

test('Use it registers Arsenal\'s bridge for the session', () => {
  const p = pi();
  assert.equal(registerArsenal(p, settings({ arsenal_mcp: BRIDGE }), () => true), BRIDGE);
  assert.equal(p.servers.length, 1);
  assert.equal(p.servers[0][0], NAME);
  assert.equal(p.servers[0][1].command, BRIDGE);
});

test('nothing without the setting, a missing bridge, or a path that is not Arsenal\'s', () => {
  for (const [value, exists] of [[{}, true], [{ arsenal_mcp: BRIDGE }, false], [{ arsenal_mcp: '/tmp/evil' }, true], [{ arsenal_mcp: 'Arsenal.app/Contents/Helpers/arsenal-mcp' }, true]]) {
    const p = pi();
    assert.equal(registerArsenal(p, settings(value), () => exists), undefined);
    assert.equal(p.servers.length, 0);
  }
  const p = pi();
  assert.equal(registerArsenal(p, undefined), undefined);
  assert.equal(registerArsenal(p, '/nonexistent/settings.json'), undefined);
  assert.equal(p.servers.length, 0);
});

test('a registration Pi refuses never throws out of the extension', () => {
  const refusing = { registerMcpServer: () => { throw new Error('name taken'); } };
  const errors = [];
  const original = console.error;
  console.error = (line) => errors.push(line);
  try {
    assert.equal(registerArsenal(refusing, settings({ arsenal_mcp: BRIDGE }), () => true), undefined);
  } finally {
    console.error = original;
  }
  assert.match(errors[0], /name taken/);
});
