/** Pi 1.4.0 bridge. Credentials live only in this process, never mcp.json. */
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';
import { convertToLlm, estimateTokens, serializeConversation } from '@earendil-works/pi-coding-agent';
import { McpClient, StreamableHttpTransport, type CallToolResult } from '@earendil-works/pi-mcp';
import { createHash } from 'node:crypto';
import { PROVIDERS, installProviderRetry } from './provider-retry.mjs';
import { registerLoopGuard } from './loop-guard.mjs';
import { registerServiceTier } from './service-tier.mjs';
import { registerQuota } from './quota.mjs';
import { registerHistoryRepair } from './history-repair.mjs';
import { registerCompactionFallback } from './compaction-fallback.mjs';
import { registerArsenal } from './arsenal.mjs';
import { closeSync, readFileSync } from 'node:fs';

export default function heimdall(pi: ExtensionAPI) {
  registerLoopGuard(pi);
  // Before the receipts below: a run the repair continues is not settled yet. Its lines are session entries that Pi
  // announces itself (`entry_appended`); nothing here writes to Pi's RPC output.
  registerHistoryRepair(pi);
  // A compaction the agent's provider refused (spent credit, quota, sign-in) is summarised by Heimdall on another model
  // and written by Pi here (compaction-fallback.mjs; bug note 2026-10-28T19-41-00).
  registerCompactionFallback(pi, { convertToLlm, estimateTokens, serializeConversation });
  // Thinking Off on a Claude model (thinking-policy.mjs): Heimdall names Anthropic's form of Off for this model; Pi
  // marks an Off request `disabled`, and the fetch wrapper rewrites it on the wire, compaction summaries included. The
  // level is Pi's own, so a change in the menu applies from the next request.
  const offMode = process.env.HEIMDALL_THINKING_OFF;
  delete process.env.HEIMDALL_THINKING_OFF;
  installProviderRetry(offMode);
  // The key goes to Pi's built-in provider in memory, then leaves the environment before any tool runs. Only lab
  // builds set these (a pasted key, or a Console access token); the installed app's Pi asks its key command instead.
  const key = process.env.HEIMDALL_OPENCODE_API_KEY || process.env.HEIMDALL_OPENCODE_TOKEN;
  if (key) pi.registerProvider('opencode-go', { apiKey: key });
  const anthropicKey = process.env.HEIMDALL_ANTHROPIC_API_KEY;
  if (anthropicKey) pi.registerProvider('anthropic', { apiKey: anthropicKey });
  for (const p of PROVIDERS) { delete process.env[p.keyEnv]; if (p.tokenEnv) delete process.env[p.tokenEnv]; }
  // A cold prompt cache is compacted by Heimdall while the agent is idle (crates/agent/src/maintenance.rs), never
  // inside a wake's prompt (19 Oct 18:12: the user's first words waited 45 s for it).
  // Speed (service-tier.mjs): Fast asks OpenAI routes for the priority tier; the requested and reported tiers are logged.
  registerServiceTier(pi, process.env.HEIMDALL_API_PROVIDER ?? '', process.env.HEIMDALL_SETTINGS_FILE, process.env.HEIMDALL_TIER_LOG);
  // Setup ▸ Arsenal ▸ Use it: Arsenal's MCP bridge for this session (arsenal.mjs); a restart picks up a change.
  registerArsenal(pi, process.env.HEIMDALL_SETTINGS_FILE);
  delete process.env.HEIMDALL_SETTINGS_FILE;
  delete process.env.HEIMDALL_TIER_LOG;
  // Plan usage limits (quota.mjs): a spent limit and its reset, and the five-hour window past 80%, go to Heimdall as
  // one `heimdall_quota` frame each; Heimdall says the line once per window and logs it under `quota:`.
  registerQuota(pi, process.env.HEIMDALL_API_PROVIDER ?? '');
  const url = process.env.HEIMDALL_MCP_URL;
  // The token arrives on a pipe Heimdall filled before launch. Reading and closing it here keeps it out of the
  // environment and out of every tool this Pi starts.
  const tokenFd = Number(process.env.HEIMDALL_MCP_TOKEN_FD);
  delete process.env.HEIMDALL_MCP_TOKEN_FD;
  const token = Number.isInteger(tokenFd) && tokenFd > 2 ? readFileSync(tokenFd, 'utf8') : undefined;
  if (Number.isInteger(tokenFd) && tokenFd > 2) closeSync(tokenFd);
  if (!url || !token || !/^http:\/\/127\.0\.0\.1:\d+\/mcp$/.test(url)) {
    throw new Error('Heimdall MCP launch credentials are missing or invalid');
  }
  let client: McpClient | undefined;
  let toolCount = 0;
  pi.on('session_start', async () => {
    client = new McpClient({ name: 'heimdall-pi', version: '1', protocolVersion: '2025-06-18', requestTimeoutMs: 60000 });
    await client.connect(new StreamableHttpTransport({ url, headers: { Authorization: `Bearer ${token}` } }));
    if (client.protocolVersion !== '2025-06-18') throw new Error('Heimdall MCP protocol mismatch');
    const tools = (await client.listTools()).sort((a, b) => a.name.localeCompare(b.name, 'en'));
    const counts = new Map<string, number>();
    for (const tool of tools) { const name = tool.name.replace(/[^a-zA-Z0-9_]/g, '_'); counts.set(name, (counts.get(name) ?? 0) + 1); }
    for (const tool of tools) {
      let name = tool.name.replace(/[^a-zA-Z0-9_]/g, '_');
      if (counts.get(name)! > 1) name += '_' + createHash('sha256').update(tool.name).digest('hex').slice(0, 10);
      pi.registerTool({
        name: 'mcp__heimdall__' + name, label: tool.name, description: tool.description ?? tool.name,
        parameters: tool.inputSchema as any, annotations: tool.annotations,
        async execute(toolCallId, args, signal) {
          if (!client || client.connectionState !== 'connected') throw new Error('Heimdall MCP disconnected; reload the agent');
          // Request ids are connection-local. This id belongs to Pi's persisted
          // tool call and stays stable across outbox/reconnection attempts.
          const result = await client.request<CallToolResult>('tools/call', {
            name: tool.name, arguments: args, _meta: { heimdallActionId: toolCallId },
          }, { signal });
          const content = result.content.filter((part) => part.type === 'text' || part.type === 'image');
          if (!content.length) content.push({ type: 'text', text: JSON.stringify(result.structuredContent ?? result) });
          if (result.isError) throw new Error(content.filter((part) => part.type === 'text').map((part) => part.text).join('\n') || 'Heimdall tool failed');
          return { content, details: { structuredContent: result.structuredContent, actionId: toolCallId } } as any;
        },
      });
    }
    toolCount = tools.length;
  });
  pi.on('session_shutdown', async () => { await client?.close(); client = undefined; });
  pi.registerCommand('heimdall_bridge_status', {
    description: 'Heimdall supervisor bridge health',
    handler: async () => { pi.appendEntry('heimdall_bridge', { connected: client?.connectionState === 'connected', protocol: client?.protocolVersion, toolCount }); },
  });
  // Context only (speech heard while the agent was off): kept in the conversation, no turn, no reply.
  pi.registerCommand('heimdall_context', {
    description: 'Heimdall context without a reply',
    handler: async (args) => { pi.sendMessage({ customType: 'heimdall_context', content: args, display: true }, { triggerTurn: false }); },
  });
  // Runtime credentials must not propagate into model-created shell children.
  delete process.env.HEIMDALL_MCP_URL;

  delete process.env.HEIMDALL_PARENT_PID;
  delete process.env.HEIMDALL_LOCK_FD;
  // The guardian owns supervisor lifetime and birth-checked descendant cleanup.
  // Pi is not the process-group leader and must never signal a group.
  // The reload outcome, recorded by Heimdall only after the replacement start is verified.
  pi.registerCommand('heimdall_reload_note', {
    description: 'Heimdall reload outcome',
    handler: async (args) => { pi.sendMessage({ customType: 'heimdall_reload', content: args, display: true }, { triggerTurn: false }); },
  });
  const consumed = new Set<string>();
  let effects = false;
  let finalReason = '';
  pi.on('message_end', async (event) => {
    const message = event.message;
    if (message.role === 'user') {
      const text = typeof message.content === 'string' ? message.content : message.content
        .filter((part) => part.type === 'text').map((part) => part.text).join('\n');
      const id = /^\[Heimdall delivery: ([a-zA-Z0-9_-]+)\]\n/.exec(text)?.[1];
      if (id) consumed.add(id);
    } else if (message.role === 'assistant') {
      finalReason = message.stopReason;
    }
  });
  pi.on('tool_call', async (event) => {
    // Instruction files are the user's (18 Oct 13:38: the agent edited the Vault AGENTS.md although its prompt forbids
    // it). Corrections are recorded by a T3 worker in Heimdall's own prompt. The shell path stays open; the
    // prompt rule covers it.
    const target = (event.toolName === 'edit' || event.toolName === 'write') ? String((event.input as any)?.path ?? '') : '';
    if (/(^|\/)(AGENTS|CLAUDE)\.md$/i.test(target)) {
      // The public edition has no source checkout or owner to name (VM finding P-04).
      const reason = process.env.HEIMDALL_EDITION === 'public'
        ? `${target} is an instruction file; you never edit those. Follow the user's correction now.`
        : `${target} is an instruction file; you never edit those. Follow User's correction now and have a T3 worker record it in crates/agent/prompt/heimdall.md.`;
      return { block: true, reason };
    }
    effects = true;
    // Persist before execution; recovery never repeats a tool-bearing wake.
    pi.appendEntry('heimdall_effects', { ids: [...consumed], toolCallId: event.toolCallId });
  });
  pi.on('agent_before_settle', async (event) => {
    // The history repair continues this run (history-repair.mjs): its receipts come when it really settles.
    if ((event as any).continue) return;
    for (const id of consumed) {
      pi.appendEntry('heimdall_receipt', {
        id, completed: event.outcome === 'completed' && finalReason === 'stop',
        effects, aborted: event.outcome === 'aborted', stopReason: finalReason,
      });
    }
    consumed.clear(); effects = false; finalReason = '';
  });
}
