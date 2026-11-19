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
          // A participant's session may run as any OpenCode primary agent, so ownership,
          // not the agent name, decides who gets the supervisor tools and prompt.
          const owned = await supervisor.isSupervisor(event.sessionID).catch(() => false);
          if (!owned) {
            for (const name of Object.keys(event.tools)) {
              if (event.agent === AGENT || name.startsWith("converse_")) delete event.tools[name];
            }
            return;
          }
          if (event.agent === AGENT)
            for (const name of Object.keys(event.tools)) {
              if (!toolIDs.has(name)) delete event.tools[name];
            }
          event.system.push({ type: "text", text: await supervisor.systemPrompt(event.sessionID) });
        }),
      );
      registrations.push(
        await ctx.rpc.register(Converse, {
          hello: async () => ({ protocol: 3 as const, packageRoot: PACKAGE_ROOT }),
          start: async (input) => supervisor.start(input?.participant),
          stop: async (input) => supervisor.stop(input.sessionID),
          attach: async (input) => supervisor.attachForUser(input.supervisorID, input.sessionID),
          detach: async (input) => supervisor.detachForUser(input.supervisorID, input.sessionID),
          turn: async (input) => supervisor.setTurnOrigin(input.supervisorID, input.origin),
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
