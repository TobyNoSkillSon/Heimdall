/** Global tool hooks: built-ins, MCP tools, and nested calls share one consecutive streak. */
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
export const policy = Object.freeze(JSON.parse(readFileSync(new URL('./loop-guard.json', import.meta.url), 'utf8')));

// Sort object keys recursively. Preserve array order, value types and string whitespace.
export function normalized(value) {
  if (Array.isArray(value)) return '[' + value.map(normalized).join(',') + ']';
  if (value && typeof value === 'object') return '{' + Object.keys(value).sort()
    .filter(key => value[key] !== undefined)
    .map(key => JSON.stringify(key) + ':' + normalized(value[key])).join(',') + '}';
  return JSON.stringify(value);
}
const hash = value => createHash('sha256').update(normalized(value)).digest('hex');

export function registerLoopGuard(pi) {
  let previous, pending, blocked = 0;
  const reset = () => { previous = pending = undefined; blocked = 0; };
  // A Pi tool round is not a user turn. Keep counts across rounds/retries until true settlement.
  pi.on('session_start', reset);
  pi.on('session_shutdown', reset);
  pi.on('agent_settled', reset);
  pi.on('tool_call', event => {
    const argsHash = hash(event.input);
    const same = previous?.tool === event.toolName && previous.argsHash === argsHash;
    // Overlapping calls have no completed identical result yet. Never invent a streak.
    if (!same || pending) previous = undefined;
    pending = undefined;
    if (previous?.repeats >= policy.identicalResults) {
      blocked++;
      pi.appendEntry('heimdall_loop_guard', {
        tool: event.toolName, arguments_hash: argsHash, repeats: previous.repeats,
        blocked_attempts: blocked, toolCallId: event.toolCallId,
      });
      return { block: true, reason: `You have called ${event.toolName} with the same arguments ${policy.identicalResults} times and got the same result. Repeating will not change it; use the result, try something different, or finish and tell ${process.env.HEIMDALL_EDITION === 'public' ? 'the user' : 'User'}.` };
    }
    pending = { id: event.toolCallId, tool: event.toolName, argsHash };
  });
  pi.on('tool_execution_end', event => {
    if (pending?.id !== event.toolCallId) return;
    // Compare the final model-visible result, including images, errors and structured output.
    // Execution metadata (duration, usage, MCP action IDs) changes on every call and is not output.
    const resultHash = hash({ content: event.result.content, isError: event.isError,
      structuredContent: Object.hasOwn(event.result, 'structuredContent')
        ? event.result.structuredContent : event.result.details?.structuredContent });
    const repeats = previous?.tool === pending.tool && previous.argsHash === pending.argsHash
      && previous.resultHash === resultHash ? previous.repeats + 1 : 1;
    previous = { ...pending, resultHash, repeats };
    pending = undefined;
  });
}
