/** Compaction on another model (bug note 2026-10-28T19-41-00 "we should have a safe compaction fallback").
 * Pi summarises only on the agent's own model; when that fails for a spent quota, credit or sign-in, Heimdall summarises
 * on the next working model itself (crates/app/src/bridge/compaction_fallback.rs) and Pi writes the entry, so the
 * session stays Pi's own format. Two commands and Pi's `session_before_compact` hook:
 * - every compaction's preparation is kept (no model call); `heimdall_compaction_source <path>` writes the newest one
 *   as text to summarise (0600): the cut Pi chose, the previous summary, the focus Heimdall asked for, a token estimate;
 * - `heimdall_compaction_supply <path>` arms the next compaction with Heimdall's result: a summary for that same cut,
 *   or, a history past every model's window) a cut that keeps the newest whole turns up to `keepTokens`
 *   and the previous summary, without summarising the rest. Pi appends its compaction entry either way: nothing leaves
 *   the session file, the context starts at the kept entry.
 * A result whose history changed (another compaction ran since the source) cancels that compaction:
 * no model call, the session as it was, and Heimdall reports it. A supply is for the compaction Heimdall starts right
 * after it (`SUPPLY_MS`); one that compaction never reached (Pi had nothing to compact) is dropped, never applied to a
 * later compaction of Pi's own. */
import { readFileSync, writeFileSync, unlinkSync } from 'node:fs';

export const SOURCE_COMMAND = 'heimdall_compaction_source';
export const SUPPLY_COMMAND = 'heimdall_compaction_supply';
export const SUPPLY_MS = 120_000;

/** The id of the newest compaction entry on the branch, `null` before the first. */
export function lastCompaction(branch) {
  for (let i = branch.length - 1; i >= 0; i--) if (branch[i]?.type === 'compaction') return branch[i].id;
  return null;
}

/** A turn starts at the user's (or Heimdall's) message or a context entry; a cut there never parts a tool call from its result. */
const turnStart = (e) => (e?.type === 'message' && e.message?.role === 'user') || e?.type === 'custom_message';

/** Pi's chars/4 estimate of what one entry adds to the context. */
function entryTokens(e, estimate) {
  if (e?.type === 'message') return estimate(e.message);
  if (e?.type === 'custom_message') return Math.ceil((typeof e.content === 'string' ? e.content : JSON.stringify(e.content ?? '')).length / 4);
  return 0;
}

/** Where what the context shows now begins: the newest compaction's kept entry, else the first entry. */
function visibleStart(branch) {
  const c = branch.map((e) => e?.type).lastIndexOf('compaction');
  if (c < 0) return 0;
  const kept = branch.findIndex((e) => e.id === branch[c].firstKeptEntryId);
  return kept >= 0 ? kept : c + 1;
}

/** The messages the context shows now, in Pi's estimate (no system prompt, tools or summary). */
export function visibleTokens(branch, estimate) {
  let n = 0;
  for (let i = visibleStart(branch); i < branch.length; i++) if (branch[i].type !== 'compaction') n += entryTokens(branch[i], estimate);
  return n;
}

/** The last-resort cut: the earliest turn start whose turns to the end fit `keepTokens`, inside what the context shows
 * now. `null` when not even the newest turn fits. */
export function cutPoint(branch, keepTokens, estimate) {
  const start = visibleStart(branch);
  let tokens = 0, best = null;
  for (let i = branch.length - 1; i >= start; i--) {
    if (branch[i].type === 'compaction') continue;
    tokens += entryTokens(branch[i], estimate);
    if (tokens > keepTokens) break;
    if (turnStart(branch[i])) best = { id: branch[i].id, tokens };
  }
  return best;
}

/** `deps`: Pi's own `serializeConversation`, `convertToLlm` and `estimateTokens` (heimdall.ts passes them); `now`, tests' clock. */
export function registerCompactionFallback(pi, deps) {
  const { serializeConversation, convertToLlm, estimateTokens, now = Date.now } = deps;
  let last;
  let supplied;
  pi.on('session_before_compact', async (event) => {
    const p = event.preparation;
    const previous = lastCompaction(event.branchEntries ?? []);
    last = { p, previous, focus: event.customInstructions };
    // Only the compaction Heimdall starts (`compact`, Pi's "manual") takes a supply, never Pi's own in a turn.
    if (!supplied || event.reason !== 'manual') return undefined;
    const s = supplied; supplied = undefined;
    if (now() - s.armedAt > SUPPLY_MS) return undefined;
    // A supply that could not be read: no summary on the provider that just failed either.
    if (s.invalid) return { cancel: true };
    const branch = event.branchEntries ?? [];
    if (s.mode === 'cut') {
      // Only the history the cut was made for (review 28 Oct): the same summary, and the source's kept entry still on
      // this branch (a new or rotated session has neither).
      if (s.previous !== undefined && s.previous !== previous) return { cancel: true };
      if (s.sourceKept !== undefined && !branch.some((e) => e.id === s.sourceKept)) return { cancel: true };
      const note = `[Heimdall, ${s.at}: the older part of this conversation was cut without a summary: the history (about ${s.tokensBefore} tokens) was past what any available model could summarise. The whole record stays in the session file.]`;
      const summary = p.previousSummary ? `${p.previousSummary}\n\n${note}` : note;
      // `keepTokens` is the whole context after the cut (review 28 Oct): the summary and Pi's fixed part (system prompt,
      // tools: what Pi's count holds beyond the messages and the old summary) come out of it, so the history lands
      // under Heimdall's compaction mark and does not compact again at once.
      const fixed = Math.max(0, (p.tokensBefore ?? 0) - visibleTokens(branch, estimateTokens) - Math.ceil((p.previousSummary ?? '').length / 4));
      const room = Math.max(0, s.keepTokens - fixed - Math.ceil(summary.length / 4));
      const cut = cutPoint(branch, room, estimateTokens) ?? { id: p.firstKeptEntryId };
      return { compaction: { summary, firstKeptEntryId: cut.id, tokensBefore: p.tokensBefore,
        details: { heimdall: 'cut', keptTokens: cut.tokens ?? null, fixedTokens: fixed, afterTokens: fixed + Math.ceil(summary.length / 4) + (cut.tokens ?? 0) } } };
    }
    if (s.previous !== previous || !branch.some((e) => e.id === s.firstKeptEntryId)) return { cancel: true };
    return { compaction: { summary: s.summary, firstKeptEntryId: s.firstKeptEntryId, tokensBefore: p.tokensBefore, details: { heimdall: s.model } } };
  });
  pi.registerCommand(SOURCE_COMMAND, {
    description: 'Heimdall: what the last compaction would summarise',
    handler: async (path) => {
      path = path.trim();
      if (!last) { writeFileSync(path, JSON.stringify({ none: true }), { mode: 0o600 }); return; }
      const { p, previous, focus } = last;
      const messages = [...(p.messagesToSummarize ?? []), ...(p.turnPrefixMessages ?? [])];
      const tokens = messages.reduce((n, m) => n + estimateTokens(m), 0) + Math.ceil((p.previousSummary ?? '').length / 4);
      writeFileSync(path, JSON.stringify({
        firstKeptEntryId: p.firstKeptEntryId, previous, tokensBefore: p.tokensBefore, tokens,
        previousSummary: p.previousSummary ?? null, focus: focus ?? null, conversation: serializeConversation(convertToLlm(messages)),
      }), { mode: 0o600 });
    },
  });
  pi.registerCommand(SUPPLY_COMMAND, {
    description: 'Heimdall: the next compaction from another model, or a cut',
    handler: async (path) => {
      path = path.trim();
      try { supplied = { ...JSON.parse(readFileSync(path, 'utf8')), armedAt: now() }; } catch { supplied = { invalid: true, armedAt: now() }; }
      try { unlinkSync(path); } catch {}
    },
  });
}
