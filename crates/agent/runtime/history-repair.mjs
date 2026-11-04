// A conversation the provider refuses never leaves the user with a dead agent (20 Oct 19:20: one JPEG labelled PNG
// in a tool result failed every request after it, across compaction and reloads).
//
// When a run ends with a 400 that names a block of the history (`messages.N…`), this extension, at Pi's
// `agent_before_settle` boundary, proposes Pi's own append-only entries and continues the same run once:
//   1. every image the provider would refuse is relabelled from its bytes or replaced by a one-line note
//      (`context_edit`), and the failed attempt is omitted; the turn then continues where it stopped;
//   2. if nothing was repairable, or the repaired request is refused again, the history restarts from a summary:
//      the last compaction summary plus the conversation since, as plain text (a compaction that keeps nothing),
//      followed by the request the run is answering, as the last message the model reads (`requestMessage`).
// A third refusal settles the run with its error. Each step is one `heimdall_history_repair` custom entry, which Pi
// itself announces to Heimdall (`entry_appended`, read by `crates/agent/src/harness.rs`): the extension never
// writes to Pi's RPC output on its own.
// The rules are the provider's documented image rules; `crates/agent/src/history.rs` applies the same ones at start.

export const MAX_IMAGE_BASE64 = 5 * 1024 * 1024;
export const MAX_SIDE = 8000;
export const MANY_IMAGES = 20;
export const MANY_IMAGES_SIDE = 2000;
/** Characters of plain-text conversation the rebuilt summary keeps (newest first), about 25K tokens. */
export const REBUILT_CHARS = 100000;

/** A 400 that names a block of the conversation history (Anthropic `messages.N.…`, OpenAI `messages[N]…`). */
export function historyRejection(text) {
  const t = String(text ?? '');
  const lower = t.toLowerCase();
  if (!(lower.includes('invalid_request') || /\b400\b/.test(t))) return false;
  // Resent by provider-retry.mjs; summarised by Pi's own overflow handling; only the user can add credit.
  if (lower.includes('signature') && lower.includes('thinking')) return false;
  if (lower.includes('prompt is too long') || lower.includes('context length') || lower.includes('credit balance')) return false;
  return /messages\.\d+\./.test(t) || /messages\[\d+\]/.test(t);
}

/** [media type, width, height] from an image's first bytes; 0 when the header does not say; null if no image. */
export function imageInfo(b) {
  if (b.length >= 8 && b.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))) {
    return ['image/png', b.length >= 24 ? b.readUInt32BE(16) : 0, b.length >= 24 ? b.readUInt32BE(20) : 0];
  }
  if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) {
    for (let i = 2; i + 3 < b.length;) {
      if (b[i] !== 0xff) break;
      const m = b[i + 1];
      if (m === 0xff) { i++; continue; }
      if (m === 0xd8 || m === 0x01 || (m >= 0xd0 && m <= 0xd7)) { i += 2; continue; }
      const len = b.readUInt16BE(i + 2);
      if (m >= 0xc0 && m <= 0xcf && m !== 0xc4 && m !== 0xc8 && m !== 0xcc) {
        return i + 9 <= b.length ? ['image/jpeg', b.readUInt16BE(i + 7), b.readUInt16BE(i + 5)] : ['image/jpeg', 0, 0];
      }
      if (m === 0xda || len < 2) break;
      i += 2 + len;
    }
    return ['image/jpeg', 0, 0];
  }
  const ascii = (s, e) => b.subarray(s, e).toString('latin1');
  if (b.length >= 10 && (ascii(0, 6) === 'GIF87a' || ascii(0, 6) === 'GIF89a')) return ['image/gif', b.readUInt16LE(6), b.readUInt16LE(8)];
  if (b.length >= 16 && ascii(0, 4) === 'RIFF' && ascii(8, 12) === 'WEBP') {
    const chunk = ascii(12, 16);
    if (chunk === 'VP8 ' && b.length >= 30) return ['image/webp', b.readUInt16LE(26) & 0x3fff, b.readUInt16LE(28) & 0x3fff];
    if (chunk === 'VP8L' && b.length >= 25) return ['image/webp', 1 + (b[21] | (b[22] & 0x3f) << 8), 1 + (b[22] >> 6 | b[23] << 2 | (b[24] & 0x0f) << 10)];
    if (chunk === 'VP8X' && b.length >= 30) return ['image/webp', 1 + b.readUIntLE(24, 3), 1 + b.readUIntLE(27, 3)];
    return ['image/webp', 0, 0];
  }
  return null;
}

const B64 = /^[A-Za-z0-9+/]*={0,2}$/;
/** What one image block needs: null, `{ relabel: type }` or `{ remove: reason }`. */
export function imageFix(block, many) {
  const data = typeof block.data === 'string' ? block.data : '';
  if (data.length > MAX_IMAGE_BASE64) return { remove: 'larger than the model accepts' };
  if (!data || !B64.test(data)) return { remove: 'its data was unreadable' };
  const info = imageInfo(Buffer.from(data, 'base64'));
  if (!info) return { remove: 'not a PNG, JPEG, GIF or WebP image' };
  const [type, w, h] = info;
  const side = Math.max(w, h);
  if (side > MAX_SIDE) return { remove: 'wider or taller than 8000 pixels' };
  if (many && side > MANY_IMAGES_SIDE) return { remove: 'over 2000 pixels in a conversation with more than 20 images' };
  return type === block.mimeType ? null : { relabel: type, from: block.mimeType };
}
export const note = (reason) => `(Image removed by Heimdall: ${reason}.)`;

const contentOf = (entry) => {
  const source = entry.sourceEntry;
  const message = entry.messages?.[0];
  if (!message) return null;
  if (source?.type === 'message' && (message.role === 'user' || message.role === 'toolResult')) return message.content;
  if (source?.type === 'custom_message') return message.content;
  return null;
};

/** Context-edit drafts that make every image in the model's view acceptable, and what they changed. */
export function imageRepairs(contextEntries) {
  const lists = contextEntries.map(contentOf).filter(Array.isArray);
  const many = lists.reduce((n, c) => n + c.filter((b) => b?.type === 'image').length, 0) > MANY_IMAGES;
  const drafts = [], relabelled = [], removed = [];
  for (const entry of contextEntries) {
    const content = contentOf(entry);
    if (!Array.isArray(content)) continue;
    let changed = false;
    const fixed = content.map((block) => {
      if (block?.type !== 'image') return block;
      const fix = imageFix(block, many);
      if (!fix) return block;
      changed = true;
      if (fix.relabel) { relabelled.push(`${fix.from} → ${fix.relabel}`); return { ...block, mimeType: fix.relabel }; }
      removed.push(fix.remove);
      return { type: 'text', text: note(fix.remove) };
    });
    if (changed) drafts.push({ type: 'context_edit', targetId: entry.sourceEntry.id, replacement: { content: fixed } });
  }
  return { drafts, relabelled, removed };
}

/** The model-visible entry that ends the context (custom, usage and other bookkeeping entries carry no messages). */
const lastVisible = (contextEntries) => contextEntries.findLast((e) => (e.messages?.length ?? 0) > 0);
/** The failed attempt that ends the context (omitted so the run can continue from the message before it). */
export function failedAttempt(contextEntries) {
  const last = lastVisible(contextEntries);
  const message = last?.messages?.at(-1);
  return last?.sourceEntry?.type === 'message' && message?.role === 'assistant' && message.stopReason === 'error' ? last.sourceEntry.id : null;
}

const text = (content) => typeof content === 'string' ? content
  : Array.isArray(content) ? content.filter((b) => b?.type === 'text').map((b) => b.text).join('\n') : '';
const cut = (s, n) => (s.length > n ? s.slice(0, n) + ' …' : s);

/** A user message as one summary line: the delivery marker goes, and the `[Now: …]` date line under it
 * (agent::clock::now_line) becomes the line's time instead of part of what was said. */
export function userLine(body) {
  const m = STAMP.exec(body);
  return `${who(m[1])}: ${body.slice(m[0].length)}`;
}
const STAMP = /^(?:\[Heimdall delivery: [^\]\n]+\]\n)?(?:\[Now: ([^\]\n]+)\]\n)?/;
// Who asked, as the summary names them: the owner in the personal edition, "User" in the public one (VM finding P-04).
const owner = () => (process.env.HEIMDALL_EDITION === 'public' ? 'User' : 'User');
const who = (when) => (when ? `${owner()} (or Heimdall's wake), ${when}` : `${owner()} (or Heimdall's wake)`);

/** A request's own words: the delivery marker goes (the `[Now: …]` line stays); a request an earlier rebuild carried
 * loses that rebuild's framing paragraph. */
const requestWords = (m) => (m.role === 'user' ? text(m.content).replace(/^\[Heimdall delivery: [^\]\n]+\]\n/, '')
  : text(m.content).split('\n\n').slice(1).join('\n\n'));
/** The custom message that carries a request across a rebuild (`requestMessage`). */
export const REQUEST_TYPE = 'heimdall_request';
const isRequest = (m) => m.role === 'user' || (m.role === 'custom' && m.customType === REQUEST_TYPE);

/** The messages the run is answering: every user message (and a request carried by an earlier rebuild) since the
 * last reply that ended its turn. A rebuilt history must still end with them, or the model reads only history and
 * stays silent. A turn ended by an abort counts as ended: a request the user stopped is never revived as the request. */
export function pendingRequests(contextEntries) {
  let pending = [];
  for (const entry of contextEntries) {
    if (entry.sourceEntry?.type === 'compaction') continue;
    for (const m of entry.messages ?? []) {
      if (m.role === 'assistant' && ['stop', 'aborted', 'length'].includes(m.stopReason)) pending = [];
      else if (isRequest(m)) pending.push(m);
    }
  }
  return pending;
}

/** The request as the last message after a rebuild: its own words (marker removed, the `[Now: …]` line kept), framed
 * as what is answered now, after the summary that lists what was already done for it. Images stay out: the model
 * refused the conversation that carried them. `null` when there is no request (a run without one). */
export function requestMessage(contextEntries) {
  const pending = pendingRequests(contextEntries);
  if (!pending.length) return null;
  const said = pending.map(requestWords).filter((t) => t.trim());
  const images = pending.reduce((n, m) => n + (Array.isArray(m.content) ? m.content.filter((b) => b?.type === 'image').length : 0), 0);
  const content = [
    'The request you are answering now. Heimdall rebuilt the conversation above from its summary because the model refused it as it was; the summary lists what was already done for this request, so continue from there without repeating it.',
    ...said,
  ];
  if (images) content.push(`(${images} image${images > 1 ? 's' : ''} sent with it ${images > 1 ? 'are' : 'is'} left out.)`);
  return { type: 'custom_message', customType: REQUEST_TYPE, content: content.join('\n\n'), display: false };
}

/** A summary that keeps nothing it cannot say in words: the last summary, then the conversation since as text. The
 * request being answered (`pendingRequests`) is not repeated here: it follows the summary (`requestMessage`). */
export function rebuiltSummary(contextEntries) {
  const first = contextEntries[0]?.sourceEntry;
  const pending = new Set(pendingRequests(contextEntries));
  const lines = [];
  for (const entry of contextEntries) {
    if (entry.sourceEntry?.type === 'compaction') continue;
    for (const m of entry.messages ?? []) {
      // Where the request came in, so the steps after it read in order; its words come once, after the summary.
      if (pending.has(m)) lines.push(`${m.role === 'user' ? who(STAMP.exec(text(m.content))[1]) : `${owner()} (carried over)`}: (the request answered now; its words follow this summary)`);
      else if (m.role === 'user') lines.push(userLine(text(m.content)));
      else if (m.role === 'assistant' && m.stopReason !== 'error') {
        const said = text(m.content);
        if (said.trim()) lines.push(`Heimdall: ${said}`);
        for (const call of (m.content ?? []).filter((b) => b?.type === 'toolCall')) lines.push(`Heimdall called ${call.name} ${cut(JSON.stringify(call.arguments ?? {}), 300)}`);
      } else if (m.role === 'toolResult') lines.push(`Result of ${m.toolName ?? 'a tool'}: ${cut(text(m.content), 600)}`);
      else if (m.role === 'custom') lines.push(`Context: ${cut(text(m.content), 3000)}`);
      else if (m.role === 'branchSummary') lines.push(`Earlier branch: ${cut(m.summary ?? '', 3000)}`);
    }
  }
  let since = lines.join('\n');
  if (since.length > REBUILT_CHARS) since = '…\n' + since.slice(since.length - REBUILT_CHARS);
  return [
    first?.type === 'compaction' ? first.summary : '(No earlier summary.)',
    '',
    '## Since that summary',
    `Heimdall rebuilt this part as plain text because the model refused the conversation as it was. Images and long tool output are left out; ${pending.size ? 'the request answered now follows this summary, and nothing else here is a request' : 'nothing here is a new request'}.`,
    since || '(nothing)',
  ].join('\n');
}

/**
 * Registers the repair at `agent_before_settle`. Register it before any handler that writes settlement receipts;
 * those must skip a boundary whose `event.continue` is true (the run goes on). Each step leaves one custom entry;
 * Pi commits it and announces it on its own RPC output (`entry_appended`), so Heimdall's log, island and Agent window
 * hear of it in order with every other event (a direct write from this handler could interleave with Pi's frames or
 * fail with EAGAIN, and a throw here would abort the repair).
 */
export function registerHistoryRepair(pi) {
  let attempts = 0;
  const say = (state, line) => ({ type: 'custom', customType: 'heimdall_history_repair', data: { state, line } });
  pi.on('agent_settled', () => { attempts = 0; });
  pi.on('session_start', () => { attempts = 0; });
  pi.on('agent_before_settle', async (event) => {
    if (event.continue || event.outcome !== 'error') return;
    const entries = event.context?.contextEntries ?? [];
    const failed = failedAttempt(entries);
    const error = lastVisible(entries)?.messages?.at(-1)?.errorMessage;
    if (!failed || !historyRejection(error)) return;
    if (attempts >= 2) {
      return { entries: [...event.entries, say('gave_up', `The model still refuses the conversation after repair: ${cut(String(error), 300)}`)] };
    }
    attempts++;
    if (attempts === 1) {
      const { drafts, relabelled, removed } = imageRepairs(entries);
      if (drafts.length) {
        const parts = [];
        if (relabelled.length) parts.push(`${relabelled.length} image${relabelled.length > 1 ? 's' : ''} relabelled (${[...new Set(relabelled)].join(', ')})`);
        if (removed.length) parts.push(`${removed.length} image${removed.length > 1 ? 's' : ''} removed (${[...new Set(removed)].join('; ')})`);
        const note = say('repaired', `The model refused the conversation; repaired it: ${parts.join('; ')}. Answering again.`);
        return { entries: [...event.entries, ...drafts, { type: 'context_edit', targetId: failed, replacement: null }, note], continue: true };
      }
      attempts = 2;
    }
    const note = say('restarted_from_summary', `The model refused the conversation (${cut(String(error), 200)}); continuing from its summary and the recent conversation as text.`);
    // The summary keeps nothing after it; the request goes after it, so the model's last message is what it answers.
    const request = requestMessage(entries);
    const rebuilt = { type: 'compaction', summary: rebuiltSummary(entries), details: { heimdall: 'history_rebuilt' } };
    return { entries: [...event.entries, rebuilt, ...(request ? [request] : []), note], continue: true };
  });
}
