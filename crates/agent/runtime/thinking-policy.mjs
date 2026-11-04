// Thinking "Off" for the Claude 5.5 models. Pi 1.4.0 sends Off as `thinking: {type: "disabled"}` with no effort and
// no per-message effort, for every model. Per Anthropic's rules Off means: Sonnet 5.5 `thinking: {type:
// "between_tools"}` (no other field, effort high or below, and no per-message effort changes), because Sonnet 5.5
// rejects `disabled`; Haiku 5.5 `thinking: {type: "disabled"}` (effort high or below). The request keeps one top-level
// effort, Low's, so Off never thinks more than Low. The rewrite runs on the wire (provider-retry.mjs), so it also
// covers compaction summaries, which never pass through Pi's `before_provider_request`.

/** The effort an Off request carries (accepted at high or below). */
export const OFF_EFFORT = 'low';

/** An effort-only system message (Pi's per-message effort). */
function effortOnly(m) {
  return m?.role === 'system' && Array.isArray(m.content) && m.content.length === 0 && m.output_config !== undefined;
}

/** The request with thinking off as `mode` ('between_tools' | 'disabled'). Only a request Pi built for Off (thinking
 *  disabled) is rewritten; anything else is returned unchanged, so thinking levels above Off are never touched. */
export function thinkingOff(payload, mode) {
  if (mode !== 'between_tools' && mode !== 'disabled') return payload;
  if (payload?.thinking?.type !== 'disabled' || !Array.isArray(payload.messages)) return payload;
  return {
    ...payload,
    thinking: { type: mode },
    output_config: { ...(payload.output_config ?? {}), effort: OFF_EFFORT },
    messages: payload.messages.filter(m => !effortOnly(m)),
  };
}
