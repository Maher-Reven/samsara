/**
 * Samsara tool shim for TypeScript agents.
 *
 * The proxy already sees every model call, because it is the configured base
 * URL. It cannot see tool calls, which usually run inside the agent's own
 * process — so this wraps them.
 *
 * The shim deliberately contains no policy. It asks the engine what to do and
 * does that. Whether we are recording or replaying, whether a fault applies,
 * how an outcome is canonicalised: all of it lives in Rust, on the other end
 * of two HTTP calls. The rule is that a shim must never be able to drift from
 * the engine, because there will eventually be one of these per language and
 * they cannot each carry a copy of the semantics.
 *
 * Usage:
 *
 * ```ts
 * import { wrapTools } from "@samsara/shim";
 *
 * const tools = wrapTools({
 *   delete_file: async ({ path }) => fs.rm(path),
 * });
 * ```
 *
 * When `SAMSARA_ENDPOINT` is unset — production, or a normal `npm test` —
 * `wrapTools` returns the tools unchanged and costs nothing.
 */

/** What the engine says to do with a tool call. */
type Decision =
  | { action: "execute"; call: string }
  | { action: "return"; call: string; outcome: Outcome };

/** How an effect resolved. Mirrors the Rust `Outcome` enum. */
export type Outcome =
  | { status: "ok"; value: unknown }
  | { status: "err"; code: string; message: string };

/** Any async tool: takes arguments, returns a result. */
export type Tool = (args: any) => Promise<unknown>;

/** Error thrown when the engine says a call failed. */
export class SamsaraToolError extends Error {
  constructor(
    public readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "SamsaraToolError";
  }
}

const endpoint = (): string | undefined => process.env.SAMSARA_ENDPOINT;

/**
 * Concurrency bookkeeping.
 *
 * The engine cannot see which tool calls the agent issued together: over HTTP
 * "concurrently in flight" and "back to back" look identical. Only this
 * process knows, so the shim reports it.
 *
 * That is not policy creeping into the shim — it is an observation only the
 * shim can make. It reports the fact; the engine decides what it means. A
 * batch id changes whenever the in-flight count returns to zero, so every
 * call in one burst of concurrency shares one, and a call made on its own
 * gets an id nobody else joins.
 */
let inFlight = 0;
let currentBatch = 0;

function enterBatch(): number {
  if (inFlight === 0) currentBatch += 1;
  inFlight += 1;
  return currentBatch;
}

function leaveBatch(): void {
  inFlight = Math.max(0, inFlight - 1);
}

async function post(path: string, body: unknown): Promise<any> {
  const base = endpoint();
  if (!base) throw new Error("samsara: no endpoint");

  const response = await fetch(`${base}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`samsara: ${path} returned ${response.status}`);
  }
  return response.json();
}

/** Turn an outcome back into a value or a throw. */
function surface(outcome: Outcome): unknown {
  if (outcome.status === "ok") return outcome.value;
  throw new SamsaraToolError(outcome.code, outcome.message);
}

/** Capture a thrown error as an outcome, so failures are recorded too. */
function capture(error: unknown): Outcome {
  if (error instanceof SamsaraToolError) {
    return { status: "err", code: error.code, message: error.message };
  }
  const message = error instanceof Error ? error.message : String(error);
  // An unclassified throw is *not* treated as ambiguous: the call never
  // reached anything, so nothing can have taken effect. Classifying it as a
  // timeout would make the duplicate-effect check hedge where it should be
  // certain.
  return { status: "err", code: "tool_error", message };
}

/** Wrap one tool. */
export function wrapTool(name: string, tool: Tool): Tool {
  if (!endpoint()) return tool;

  return async (args: any) => {
    const batch = enterBatch();
    try {
      const decision: Decision = await post("/begin", {
        name,
        body: args ?? null,
        batch,
      });

      // Replaying, or a fault applies: the engine already knows the answer
      // and the real tool must not run. This is what makes replay free and
      // safe — no file is deleted twice while you are debugging why a file
      // was deleted twice.
      if (decision.action === "return") {
        return surface(decision.outcome);
      }

      let outcome: Outcome;
      try {
        outcome = { status: "ok", value: await tool(args) };
      } catch (error) {
        outcome = capture(error);
      }

      // `call` correlates this result with its begin. Without it two
      // concurrent calls would race to claim each other's outcomes, which is
      // a bug you would only ever see under the exact conditions this
      // release exists to reproduce.
      const { outcome: final } = await post("/end", {
        call: decision.call,
        outcome,
      });
      return surface(final);
    } finally {
      leaveBatch();
    }
  };
}

/** Wrap a whole tool map. */
export function wrapTools<T extends Record<string, Tool>>(tools: T): T {
  if (!endpoint()) return tools;

  return Object.fromEntries(
    Object.entries(tools).map(([name, tool]) => [name, wrapTool(name, tool)]),
  ) as T;
}

/** Whether Samsara is attached to this process. */
export function isAttached(): boolean {
  return endpoint() !== undefined;
}

/** Reset concurrency bookkeeping. Exposed for tests. */
export function __resetBatches(): void {
  inFlight = 0;
  currentBatch = 0;
}
