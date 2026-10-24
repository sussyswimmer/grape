/**
 * Guarded event wiring shared by the overlay and the toast. Every payload
 * arrives as `unknown` and reaches its handler only after its guard from
 * `events.ts` accepts it.
 */

import { listen } from "@tauri-apps/api/event";

/** Tauri's `listen`, narrowed to what the wiring uses (a stand-in in tests). */
export type ListenFn = <T>(
  event: string,
  handler: (event: { payload: T }) => void,
) => Promise<() => void>;

/** One event name and what to do with its (still unchecked) payloads. */
export interface Route {
  readonly event: string;
  deliver(payload: unknown): void;
}

/**
 * A route that passes payloads accepted by `guard` to `handle` and logs the
 * rest. The rejected payload itself is never logged.
 */
export function route<T>(
  event: string,
  guard: (value: unknown) => value is T,
  handle: (payload: T) => void,
): Route {
  return {
    event,
    deliver(payload) {
      if (guard(payload)) {
        handle(payload);
      } else {
        console.warn(`Grape: ignored a ${event} event whose payload doesn't match the contract`);
      }
    },
  };
}

/**
 * Listens for every route and resolves to one function that stops them all.
 *
 * Never rejects: if any `listen` fails (for example outside Tauri), it logs
 * `failureMessage` with the error, stops the listeners that did start, and
 * resolves to a no-op. So `void listenAll(…)` is safe.
 */
export async function listenAll(
  routes: readonly Route[],
  failureMessage: string,
  listenFn: ListenFn = listen,
): Promise<() => void> {
  const results = await Promise.allSettled(
    routes.map((r) => {
      try {
        return listenFn<unknown>(r.event, ({ payload }) => r.deliver(payload));
      } catch (error: unknown) {
        return Promise.reject(error);
      }
    }),
  );
  const unlisteners: (() => void)[] = [];
  const failures: unknown[] = [];
  for (const result of results) {
    if (result.status === "fulfilled") {
      unlisteners.push(result.value);
    } else {
      failures.push(result.reason);
    }
  }
  const stopAll = (): void => {
    for (const unlisten of unlisteners.splice(0)) {
      try {
        unlisten();
      } catch {
        // Already gone; nothing else to do.
      }
    }
  };
  if (failures.length > 0) {
    console.error(failureMessage, failures[0]);
    stopAll();
    return () => undefined;
  }
  return stopAll;
}
