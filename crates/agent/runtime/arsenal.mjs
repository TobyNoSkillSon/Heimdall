// Setup ▸ Arsenal ▸ Use it (Heimdall's settings.json `arsenal_mcp`): Arsenal's own MCP bridge, the
// `Contents/Helpers/arsenal-mcp` inside Arsenal.app, which finds the running app and its token by itself. Registered
// for this session only (Pi's `registerMcpServer`), never written to the user's mcp.json; a server of the same name
// there wins. Nothing happens without the setting, or when the bridge is not where Arsenal installs it.
import { existsSync, readFileSync } from 'node:fs';

export const NAME = 'arsenal';
export const DESCRIPTION = 'Arsenal: on-device models on this Mac (transcription, voices, documents, translation, images, vision, sound and music, embeddings), called as tools.';
const BRIDGE = /^\/.+\/Arsenal\.app\/Contents\/Helpers\/arsenal-mcp$/;

/** The bridge the settings name, when it is Arsenal's and on disk; else undefined. */
export function arsenalBridge(settingsFile, exists = existsSync) {
  if (!settingsFile) return undefined;
  let bridge;
  try { bridge = JSON.parse(readFileSync(settingsFile, 'utf8')).arsenal_mcp; } catch { return undefined; }
  return typeof bridge === 'string' && BRIDGE.test(bridge) && exists(bridge) ? bridge : undefined;
}

export function registerArsenal(pi, settingsFile, exists = existsSync) {
  const command = arsenalBridge(settingsFile, exists);
  if (!command) return undefined;
  // Arsenal is an extra: a registration Pi refuses (a name another extension holds, a config it rejects) never takes
  // Heimdall's extension down with it.
  try {
    pi.registerMcpServer(NAME, { command, args: [], description: DESCRIPTION });
  } catch (e) {
    console.error(`Arsenal not registered: ${e?.message ?? e}`);
    return undefined;
  }
  return command;
}
