import { EventEmitter } from "node:events";
import { PassThrough, Writable } from "node:stream";
import { startRuntime } from "../dist/client/runtime.js";

/** Real bridge/transport, fake SDK and child; never opens audio or loads models. */
export function cleanupFixture() {
  const calls = [];
  const processes = [];
  let session = 0;
  let publish;
  const client = {
    plugin: {
      list: async () => ({
        data: [
          { id: "converse", features: { server: true, rpc: true }, state: { status: "active" } },
        ],
      }),
    },
    rpc() {
      return {
        hello: async () => ({ protocol: 1 }),
        start: async () => ({ sessionID: `session-${++session}` }),
        stop: async (input, options) => {
          calls.push({ input, location: options.location });
          throw new Error("original-location server cleanup refused");
        },
      };
    },
    event: {
      subscribe({ signal }) {
        const pending = [{ value: { type: "server.connected", data: {} }, done: false }];
        let next;
        publish = (event) => {
          const entry = { value: event, done: false };
          if (next) {
            const resolve = next;
            next = undefined;
            resolve(entry);
          } else pending.push(entry);
        };
        signal.addEventListener(
          "abort",
          () => {
            next?.({ done: true });
            next = undefined;
          },
          { once: true },
        );
        return {
          [Symbol.asyncIterator]() {
            return this;
          },
          next() {
            return signal.aborted
              ? Promise.resolve({ done: true })
              : pending.length
                ? Promise.resolve(pending.shift())
                : new Promise((resolve) => {
                    next = resolve;
                  });
          },
        };
      },
    },
  };
  return {
    client,
    calls,
    processes,
    move() {
      publish({
        type: "session.moved",
        location: { directory: "/chosen" },
        data: {
          sessionID: `session-${session}`,
          location: { directory: "/moved" },
          projectID: "project",
        },
      });
    },
    runtime(options) {
      const proc = new EventEmitter();
      proc.stdout = new PassThrough();
      proc.stderr = new PassThrough();
      proc.exitOnShutdown = true;
      proc.stdin = new Writable({
        write(chunk, _encoding, done) {
          const frame = JSON.parse(chunk.toString());
          if (frame.type === "init") proc.stdout.write('{"type":"status","state":"listening"}\n');
          if (frame.type === "shutdown" && proc.exitOnShutdown)
            queueMicrotask(() => proc.emit("close", 0, null));
          done();
        },
      });
      proc.kill = () => false;
      processes.push(proc);
      queueMicrotask(() => proc.stdout.write('{"type":"hello","protocol":1}\n'));
      return startRuntime({
        ...options,
        spawn: () => proc,
        shutdownTimeoutMs: 2,
        killTimeoutMs: 2,
      });
    },
  };
}
