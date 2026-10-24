/**
 * UI → Rust commands. The UI only ever sends ids and seqs, never paths.
 */

import { invoke } from "@tauri-apps/api/core";

/** Calls a Rust command (Tauri's `invoke`, or a stand-in in tests). */
export type InvokeFn = (command: string, args: Record<string, unknown>) => Promise<unknown>;

/** Tauri's `invoke`. */
export const tauriInvoke: InvokeFn = invoke;

/**
 * Fires a command and forgets it: a failure (no IPC outside Tauri, a
 * command Rust doesn't know yet) is swallowed, whether it throws or rejects,
 * so it can never break drawing.
 */
export function callQuietly(
  invokeFn: InvokeFn,
  command: string,
  args: Record<string, unknown>,
): void {
  try {
    invokeFn(command, args).catch(() => undefined);
  } catch {
    // Synchronous failure: same as a rejection.
  }
}
