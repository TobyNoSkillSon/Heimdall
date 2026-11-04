// Speed (Standard vs Fast, "no automatic paid tier changes"). The `settings` tool's
// agent_speed is read from settings.json per request, so a change applies from the next request. Fast asks OpenAI's
// Responses API for `service_tier: "priority"` (the field Codex 0.163 sends for its Fast tier); Standard sends no tier.
// Only OpenAI routes get it (the Codex subscription, an OpenAI API key); every other provider's request is unchanged.
// Each completed response's reported tier is logged beside the requested one. A reported "default" after a priority
// request is logged as exactly that: it is not proof of which mode served the request.
import { readFileSync, appendFileSync } from 'node:fs';

export const PRIORITY = 'priority';
export const OPENAI_PROVIDERS = ['openai', 'openai-api'];

/** The tier to request: 'priority' when settings.json says agent_speed "fast" and the provider is OpenAI's, else none. */
export function requestedTier(settingsText, provider) {
  if (!OPENAI_PROVIDERS.includes(provider)) return undefined;
  try {
    return JSON.parse(settingsText)?.agent_speed === 'fast' ? PRIORITY : undefined;
  } catch {
    return undefined;
  }
}

/** A Responses request with the tier set; any other payload (or no tier) is returned unchanged. */
export function withTier(payload, tier) {
  if (!tier || !payload || typeof payload !== 'object' || !('input' in payload) || !('model' in payload)) return payload;
  return { ...payload, service_tier: tier };
}

/** One log record for a Responses stream event that reports a tier (`response.created`, `response.completed`). */
export function tierRecord(event, requested, now = new Date()) {
  const type = event?.type;
  if (type !== 'response.created' && type !== 'response.completed') return undefined;
  const r = event.response ?? {};
  return { at: now.toISOString(), event: type, model: r.model ?? null, response_id: r.id ?? null,
    requested: requested ?? 'standard', reported: r.service_tier ?? null };
}

/** Registers the hooks on `pi` for `provider`, reading `settingsFile` and appending records to `logFile`. */
export function registerServiceTier(pi, provider, settingsFile, logFile) {
  if (!OPENAI_PROVIDERS.includes(provider) || !settingsFile) return;
  let requested;
  pi.on('before_provider_request', (event) => {
    let text = '';
    try { text = readFileSync(settingsFile, 'utf8'); } catch {}
    requested = requestedTier(text, provider);
    return requested ? withTier(event.payload, requested) : undefined;
  });
  if (!logFile) return;
  pi.on('provider_stream_event', (event) => {
    const record = tierRecord(event.data, requested);
    if (record) { try { appendFileSync(logFile, JSON.stringify(record) + '\n'); } catch {} }
  });
}
