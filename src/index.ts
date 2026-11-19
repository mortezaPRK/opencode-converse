import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { Plugin } from "@opencode/plugin";
import { Converse } from "./rpc.js";
import {
  AGENT,
  Supervisor,
  SUPERVISOR_PROMPT,
  TOOL_DESCRIPTIONS,
  TOOL_SCHEMAS,
  WORKER_AGENT,
  WORKER_PROMPT,
  parseOptions,
  watchWorkers,
  type ToolName,
} from "./supervisor.js";

/** dist/index.js -> package root. */
const PACKAGE_ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

export default Plugin.define({
  id: "converse",
  async setup(ctx) {
    const supervisor = new Supervisor(ctx, parseOptions(ctx.options));
    const toolIDs = new Set(Object.keys(TOOL_SCHEMAS).map((name) => `converse_${name}`));
    const registrations: { dispose(): Promise<void> }[] = [];
    const controller = new AbortController();
    try {
      registrations.push(
        await ctx.agent.transform((editor) => {
          editor.update(AGENT, (agent) => {
            agent.description = "Live spoken supervisor for OpenCode Converse";
            agent.mode = "primary";
            agent.hidden = true;
            agent.system = SUPERVISOR_PROMPT;
            agent.permissions = [
              { action: "*", resource: "*", effect: "deny" },
              { action: "converse", resource: "*", effect: "allow" },
            ];
          });
          editor.update(WORKER_AGENT, (agent) => {
            agent.description = "Coding worker for OpenCode Converse voice sessions";
            agent.mode = "primary";
            agent.hidden = true;
            agent.system = WORKER_PROMPT;
            agent.permissions = [
              { action: "*", resource: "*", effect: "allow" },
              { action: "converse", resource: "*", effect: "deny" },
              { action: "read", resource: "*.env", effect: "ask" },
              { action: "read", resource: "*.env.*", effect: "ask" },
              { action: "read", resource: "*.env.example", effect: "allow" },
            ];
          });
        }),
      );
      registrations.push(
        await ctx.tool.transform((editor) => {
          editor.namespace({
            name: "converse",
            description: "Converse supervisor's owned coding workers",
          });
          for (const name of Object.keys(TOOL_SCHEMAS) as ToolName[]) {
            editor.add({
              name,
              description: TOOL_DESCRIPTIONS[name],
              input: TOOL_SCHEMAS[name],
              options: { namespace: "converse", codemode: false, permission: "converse" },
              execute: async (input, context) => ({
                content: await supervisor.execute(name, input, context.sessionID, context.signal),
              }),
            });
          }
        }),
      );
      registrations.push(
        await ctx.session.hook("context", async (event) => {
          if (event.agent !== AGENT) {
            for (const name of Object.keys(event.tools)) {
              if (name.startsWith("converse_")) delete event.tools[name];
            }
            return;
          }
          const owned = await supervisor.isSupervisor(event.sessionID).catch(() => false);
          for (const name of Object.keys(event.tools)) {
            if (!owned || !toolIDs.has(name)) delete event.tools[name];
          }
          if (owned) event.system.push({ type: "text", text: SUPERVISOR_PROMPT });
        }),
      );
      registrations.push(
        await ctx.rpc.register(Converse, {
          hello: async () => ({ protocol: 1 as const, packageRoot: PACKAGE_ROOT }),
          start: async () => supervisor.start(),
          stop: async (input) => supervisor.stop(input.sessionID),
        }),
      );
    } catch (error) {
      await Promise.allSettled(
        registrations.reverse().map((registration) => registration.dispose()),
      );
      throw error;
    }
    const stream = watchWorkers(ctx.event, supervisor, controller.signal);
    return async () => {
      controller.abort();
      await Promise.allSettled(
        registrations.reverse().map((registration) => registration.dispose()),
      );
      await stream;
    };
  },
});
