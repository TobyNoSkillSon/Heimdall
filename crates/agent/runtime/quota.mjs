// Plan usage limits. 26 Oct 23:33 the ChatGPT plan's limit ran out: every wake failed with
// "The ChatGPT user has reached their Subscription Sharing usage limit…", Heimdall resent each one once, and nothing
// told the user which limit it was or when it came back (Codex's own log showed the five-hour window resetting at 03:04).
// This module reads what OpenAI actually sends on the agent's requests and tells Heimdall in one frame per event:
//   - a usage-limit failure: which limit and when it resets, from the error body or the rate-limit headers;
//     no reset time at all means a line without a time, never a guess;
//   - the five-hour window passing WARN_PERCENT on a successful response.
// Each is said once per limit window (the app speaks it and logs it under `quota:`); a usage-limit failure is never
// resent (crates/agent/src/notice.rs `quota_exhausted`).
//
// Data it understands (Codex CLI 0.163 reads the same families from the ChatGPT backend):
//   headers `x-<limit>-primary-used-percent`, `-primary-window-minutes`, `-primary-reset-at` (epoch seconds) or
//   `-primary-reset-after-seconds`, the same for `secondary`; `retry-after` / `retry-after-ms` on a 429;
//   error bodies `{"error":{"code"|"type", "message", "resets_at", "resets_in_seconds"}}` (HTTP or an SSE `error` /
//   `response.failed` event), and `rate_limits: {primary, secondary}` objects ({used_percent, window_minutes,
//   resets_at}) in a body or stream event.
import { writeSync } from 'node:fs';

export const PROVIDERS = { openai: 'ChatGPT', 'openai-api': 'OpenAI' };
export const WARN_PERCENT = 80;
export const FIVE_HOURS_MIN = 300;
const OPENAI_URL = /^https:\/\/(?:api\.openai\.com|chatgpt\.com)\//;
// Plan quota codes and wording. A plain "rate limit" (a per-minute throttle) is not one.
const USAGE_LIMIT = /usage[ _]limit|usage_limit_reached|subscription_sharing_usage_limit_exceeded|insufficient_quota|quota[ _]exceeded/i;

/** The text (an error message, code or body) says a plan usage limit or quota is spent. */
export function isUsageLimit(text) {
  return USAGE_LIMIT.test(String(text ?? ''));
}

function entries(headers) {
  if (!headers) return [];
  if (typeof headers.entries === 'function' && typeof headers.get === 'function') return [...headers.entries()];
  return Object.entries(headers);
}
const num = (v) => { const n = Number(v); return v !== null && v !== undefined && v !== '' && Number.isFinite(n) ? n : undefined; };
const epochMs = (s) => { const n = num(s); return n === undefined ? undefined : (n > 1e12 ? n : n * 1000); };

/** Rate-limit windows and a stated retry delay from response headers (a Headers object or a plain record). */
export function headerLimits(headers, now = Date.now()) {
  const groups = new Map();
  let retryAfterMs, reachedType;
  for (const [rawKey, value] of entries(headers)) {
    const key = rawKey.toLowerCase();
    const m = /^x-([a-z0-9_-]+?)-(primary|secondary)-(used-percent|window-minutes|reset-at|reset-after-seconds)$/.exec(key);
    if (m) {
      const id = `${m[1]}:${m[2]}`;
      const w = groups.get(id) ?? { limit: m[1], slot: m[2] };
      if (m[3] === 'used-percent') w.usedPercent = num(value);
      else if (m[3] === 'window-minutes') w.windowMinutes = num(value);
      else if (m[3] === 'reset-at') w.resetsAtMs = epochMs(value) ?? w.resetsAtMs;
      else if (w.resetsAtMs === undefined && num(value) !== undefined) w.resetsAtMs = now + num(value) * 1000;
      groups.set(id, w);
    } else if (key === 'retry-after-ms' && num(value) > 0) {
      retryAfterMs = num(value);
    } else if (key === 'retry-after' && retryAfterMs === undefined) {
      const s = num(value);
      const date = s === undefined ? Date.parse(value) : NaN;
      if (s !== undefined && s >= 0) retryAfterMs = s * 1000;
      else if (Number.isFinite(date)) retryAfterMs = Math.max(0, date - now);
    } else if (/^x-[a-z0-9_-]+-rate-limit-reached-type$/.test(key)) {
      reachedType = String(value);
    }
  }
  // Codex's own family first when several are present.
  const windows = [...groups.values()].sort((a, b) => (a.limit === 'codex' ? 0 : 1) - (b.limit === 'codex' ? 0 : 1));
  return { windows, retryAfterMs, reachedType };
}

function windowsFrom(rateLimits, now) {
  if (!rateLimits || typeof rateLimits !== 'object') return [];
  return ['primary', 'secondary'].flatMap((slot) => {
    const w = rateLimits[slot];
    if (!w || typeof w !== 'object') return [];
    const resetsAtMs = epochMs(w.resets_at) ?? (num(w.resets_in_seconds ?? w.reset_after_seconds) !== undefined ? now + num(w.resets_in_seconds ?? w.reset_after_seconds) * 1000 : undefined);
    return [{ limit: rateLimits.limit_id ?? 'codex', slot, usedPercent: num(w.used_percent), windowMinutes: num(w.window_minutes), resetsAtMs }];
  });
}

/** The code, message, stated reset and windows of an error body (JSON text or object): HTTP error, SSE `error` or
 * `response.failed` data. Undefined when it is not JSON. */
export function bodyLimits(body, now = Date.now()) {
  let v = body;
  if (typeof v === 'string') { try { v = JSON.parse(v); } catch { return undefined; } }
  if (!v || typeof v !== 'object') return undefined;
  const err = (v.error && typeof v.error === 'object') ? v.error : (v.response?.error && typeof v.response.error === 'object') ? v.response.error : v;
  const pick = (k) => err[k] ?? v[k];
  const seconds = num(pick('resets_in_seconds') ?? pick('reset_after_seconds') ?? pick('retry_after_seconds'));
  const resetsAtMs = epochMs(pick('resets_at')) ?? (seconds !== undefined ? now + seconds * 1000 : undefined);
  const code = [err.code, err.type, v.code].find((c) => typeof c === 'string' && c && c !== 'error');
  const message = typeof err.message === 'string' ? err.message : typeof v.message === 'string' ? v.message : undefined;
  return { code, message, resetsAtMs, windows: windowsFrom(pick('rate_limits'), now) };
}

const WORDS = ['', 'one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight', 'nine', 'ten', 'eleven', 'twelve'];
/** "five-hour", "weekly", "daily"; undefined for a window it cannot name. */
export function windowName(minutes) {
  if (minutes === 10080) return 'weekly';
  if (minutes === 1440) return 'daily';
  if (minutes > 0 && minutes % 60 === 0 && minutes / 60 < WORDS.length) return `${WORDS[minutes / 60]}-hour`;
  return undefined;
}

/** Local clock time for speech: "3:04", "15:04"; a reset more than 18 hours away also names the day. */
export function clockText(ms, now = Date.now()) {
  const d = new Date(ms);
  const time = `${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`;
  return ms - now > 18 * 3600_000 ? `on ${d.toLocaleDateString('en-GB', { weekday: 'long' })} at ${time}` : `at ${time}`;
}

/** The one spoken line for a spent limit. */
export function limitLine(name, windowMinutes, resetsAtMs, now = Date.now()) {
  const which = windowName(windowMinutes) ?? 'usage';
  return resetsAtMs === undefined
    ? `${name}'s ${which} limit is used up.`
    : `${name}'s ${which} limit is used up; it resets ${clockText(resetsAtMs, now)}.`;
}

/** The one spoken line when the five-hour window passes WARN_PERCENT. */
export function warningLine(name, window, now = Date.now()) {
  const which = windowName(window.windowMinutes) ?? 'five-hour';
  const at = window.resetsAtMs === undefined ? '' : `; it resets ${clockText(window.resetsAtMs, now)}`;
  return `${name}'s ${which} limit is ${Math.floor(window.usedPercent)}% used${at}.`;
}

/** Which limit is spent and when it resets, from the failure's body first, then the headers. Never guessed: a window
 * counts only at 100% used; a 429's retry-after is the provider's own stated time. */
export function resolveLimit(error, snapshot) {
  const exhausted = (ws) => ws.filter((w) => w.usedPercent >= 100).sort((a, b) => (b.resetsAtMs ?? 0) - (a.resetsAtMs ?? 0))[0];
  const known = [...(error?.windows ?? []), ...(snapshot?.windows ?? [])];
  if (error?.resetsAtMs !== undefined) {
    // Name the window whose reset matches the stated one.
    const w = known.find((x) => x.resetsAtMs !== undefined && Math.abs(x.resetsAtMs - error.resetsAtMs) < 120_000);
    return { resetsAtMs: error.resetsAtMs, windowMinutes: w?.windowMinutes, source: 'body' };
  }
  const fromBody = exhausted(error?.windows ?? []);
  if (fromBody) return { resetsAtMs: fromBody.resetsAtMs, windowMinutes: fromBody.windowMinutes, source: 'body' };
  const fromHeaders = exhausted(snapshot?.windows ?? []);
  if (fromHeaders) return { resetsAtMs: fromHeaders.resetsAtMs, windowMinutes: fromHeaders.windowMinutes, source: 'header' };
  if (snapshot?.retryAfterMs !== undefined && snapshot.status === 429) return { resetsAtMs: snapshot.at + snapshot.retryAfterMs, windowMinutes: undefined, source: 'header' };
  return { resetsAtMs: undefined, windowMinutes: undefined, source: 'none' };
}

/** The once-per-window state. `emit` gets each frame; `now` is the clock. */
export function createQuotaWatch({ provider, emit, now = Date.now }) {
  const name = PROVIDERS[provider] ?? provider;
  let snapshot;          // the latest response's headers: { at, status, windows, retryAfterMs, reachedType }
  let error;             // the latest failure body: { code, message, resetsAtMs, windows }
  let limitUntil = null; // a limit was said: its reset (ms), or 'episode' until a success when no reset is known
  let limitKey;          // that limit's identity for the app's once-only receipt
  let episode = 0;       // limits ended by a success in this process (a new limit after one is said again)
  let warnedUntil = null;
  let pendingWarning;
  let scans = new Set();

  const merge = (windows) => {
    if (!windows?.length) return;
    snapshot = { ...(snapshot ?? { at: now(), status: 200, retryAfterMs: undefined }), windows };
  };
  const watch = {
    /** A new OpenAI request starts: an earlier failure's body no longer describes what happens next. */
    request() { error = undefined; },
    /** A response's status and headers (every OpenAI request, errors included). */
    response(status, headers) {
      const t = now();
      const h = headerLimits(headers, t);
      snapshot = { at: t, status, ...h, windows: h.windows.length ? h.windows : (status < 400 ? [] : snapshot?.windows ?? []) };
      if (status < 400) { error = undefined; watch.checkWarning(); }
    },
    /** An error body: an HTTP error's text or an SSE error event's data. */
    errorBody(body) {
      const parsed = bodyLimits(body, now());
      if (parsed) { error = parsed; merge(parsed.windows); }
    },
    /** A parsed stream event (`provider_stream_event`): rate-limit snapshots some backends send in the stream. */
    streamEvent(data) {
      if (!data || typeof data !== 'object') return;
      const rl = data.rate_limits ?? (String(data.type ?? '').endsWith('rate_limits') ? data : undefined);
      const ws = windowsFrom(rl, now());
      if (ws.length) { merge(ws); watch.checkWarning(); }
      if (data.type === 'error' || data.type === 'response.failed') watch.errorBody(data);
    },
    checkWarning() {
      const t = now();
      if (limitUntil !== null) return;
      const w = (snapshot?.windows ?? []).find((x) => x.slot === 'primary' && (x.windowMinutes === undefined || x.windowMinutes === FIVE_HOURS_MIN));
      if (!w || !(w.usedPercent >= WARN_PERCENT) || w.usedPercent >= 100) return;
      if (warnedUntil !== null && t < warnedUntil) return;
      warnedUntil = w.resetsAtMs ?? t + FIVE_HOURS_MIN * 60_000;
      pendingWarning = { type: 'heimdall_quota', event: 'warning', provider, line: warningLine(name, w, t),
        used_percent: w.usedPercent, window_minutes: w.windowMinutes ?? null, resets_at_ms: w.resetsAtMs ?? null, source: 'header',
        key: `warning:${provider}:${Math.round(warnedUntil / 60_000)}`, repeat: false };
    },
    track(promise) { scans.add(promise); promise.finally(() => scans.delete(promise)); },
    async settled(ms = 1000) {
      if (scans.size) await Promise.race([Promise.allSettled([...scans]), new Promise((r) => setTimeout(r, ms).unref?.())]);
    },
    /** An assistant message ended (`message_end`): a usage-limit failure says its line once per window; a success
     * clears the limit and says a pending warning. */
    async messageEnd(message) {
      if (message?.role !== 'assistant') return;
      if (message.stopReason === 'aborted') return;
      if (message.stopReason !== 'error') {
        if (limitUntil !== null) episode += 1;
        limitUntil = null;
        if (pendingWarning) { emit(pendingWarning); pendingWarning = undefined; }
        return;
      }
      await watch.settled();
      const text = `${message.errorMessage ?? ''} ${error?.code ?? ''} ${error?.message ?? ''}`;
      if (!isUsageLimit(text)) return;
      const t = now();
      const found = resolveLimit(error, snapshot);
      const repeat = limitUntil !== null && (limitUntil === 'episode' || t < limitUntil);
      if (!repeat) {
        limitUntil = found.resetsAtMs ?? 'episode';
        limitKey = `limit:${provider}:${found.resetsAtMs === undefined ? `unknown-${Math.floor(t / (5 * 3_600_000))}-${episode}` : Math.round(found.resetsAtMs / 60_000)}`;
      }
      pendingWarning = undefined;
      emit({ type: 'heimdall_quota', event: 'limit', provider, line: limitLine(name, found.windowMinutes, found.resetsAtMs, t),
        window_minutes: found.windowMinutes ?? null, resets_at_ms: found.resetsAtMs ?? null, source: found.source,
        code: error?.code ?? null, status: snapshot?.status ?? null,
        key: limitKey, repeat });
    },
  };
  return watch;
}

/** Reads every SSE block of `body` (a ReadableStream of bytes) and hands error events' data to `onError`. Only blocks
 * that mention an error are parsed. */
export async function scanStream(body, onError) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let cut;
      while ((cut = buffer.search(/\r?\n\r?\n/)) >= 0) {
        const block = buffer.slice(0, cut);
        buffer = buffer.slice(cut).replace(/^\r?\n\r?\n/, '');
        if (!block.includes('error') && !block.includes('response.failed')) continue;
        const data = block.split(/\r?\n/).filter((l) => l.startsWith('data:')).map((l) => l.slice(5).trimStart()).join('\n');
        const event = /^event:\s*(.*)$/m.exec(block)?.[1];
        let parsed;
        try { parsed = JSON.parse(data); } catch { continue; }
        if (event === 'error' || parsed?.type === 'error' || parsed?.type === 'response.failed' || parsed?.error) onError(parsed);
      }
    }
  } catch {
    // The consumer stopped the stream (the SDK aborts after an error event): what was read is enough.
  } finally {
    try { reader.releaseLock(); } catch {}
  }
}

/** Wraps `fetch`: each OpenAI POST's status and headers go to `watch`, an error's body too, and a successful stream is
 * read on a clone for error events. The response itself is returned untouched. */
export function quotaFetch(fetch, watch) {
  return async function (input, init) {
    const url = typeof input === 'string' || input instanceof URL ? String(input) : input?.url ?? '';
    const method = String(init?.method ?? input?.method ?? 'GET').toUpperCase();
    const watched = method === 'POST' && OPENAI_URL.test(url);
    if (watched) watch.request();
    const response = await fetch(input, init);
    if (!watched) return response;
    try {
      watch.response(response.status, response.headers);
      if (response.status >= 400) {
        const text = await response.clone().text().catch(() => '');
        watch.errorBody(text);
      } else if ((response.headers.get('content-type') ?? '').includes('text/event-stream') && response.body) {
        watch.track(scanStream(response.clone().body, (data) => watch.errorBody(data)));
      }
    } catch {
      // Observation never breaks a request.
    }
    return response;
  };
}

/** Registers the watch on `pi` for `provider` (OpenAI routes only). Frames go to the RPC descriptor like
 * provider-retry.mjs's (Pi redirects process.stdout during extension calls). */
export function registerQuota(pi, provider, emit = (frame) => writeSync(1, JSON.stringify(frame) + '\n'), now = Date.now) {
  if (!PROVIDERS[provider]) return undefined;
  const watch = createQuotaWatch({ provider, emit, now });
  globalThis.fetch = quotaFetch(globalThis.fetch, watch);
  pi.on('provider_stream_event', (event) => { watch.streamEvent(event?.data); });
  pi.on('message_end', async (event) => { await watch.messageEnd(event?.message); });
  return watch;
}
