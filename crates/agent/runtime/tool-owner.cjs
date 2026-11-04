// Loaded before Pi. All child spawns inherit the supervisor generation, including
// tools with an explicit env. Descendants retain it through setsid/double fork.
const cp = require('node:child_process');
const { syncBuiltinESMExports } = require('node:module');
const token = process.env.HEIMDALL_AGENT_OWNER;
if (!token) throw new Error('Missing Heimdall process ownership token');
for (const name of ['spawn', 'spawnSync']) {
  const original = cp[name];
  cp[name] = function(command, args, options) {
    if (!Array.isArray(args)) { options = args; args = []; }
    const env = {...(options?.env ?? process.env), HEIMDALL_AGENT_OWNER: token};
    for (const name of Object.keys(env)) if (/^HEIMDALL_.*_API_KEY$|^HEIMDALL_OPENCODE_TOKEN$/.test(name)) delete env[name];
    return original.call(this, command, args, {...options, env});
  };
}
syncBuiltinESMExports();
