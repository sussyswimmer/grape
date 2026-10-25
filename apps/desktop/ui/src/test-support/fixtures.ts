/**
 * The golden payloads from `src/shared/fixtures/`, parsed and guarded, for
 * tests. Test code only.
 */

import {
  type FloatView,
  isFloatView,
  isToastShowPayload,
  isWheelOpenPayload,
  type ToastShowPayload,
  type WheelOpenPayload,
} from "../shared/events";
import { isSettingsView, type SettingsView } from "../settings/model";

const raw = import.meta.glob<string>("../shared/fixtures/*.json", {
  query: "?raw",
  import: "default",
  eager: true,
});

/** The parsed JSON of `src/shared/fixtures/<name>`. */
export function fixtureJson(name: string): unknown {
  const text = raw[`../shared/fixtures/${name}`];
  if (text === undefined) {
    throw new Error(`no fixture ${name}`);
  }
  return JSON.parse(text);
}

/** A `wheel://open` fixture, guarded. */
export function openFixture(name: string): WheelOpenPayload {
  const value = fixtureJson(name);
  if (!isWheelOpenPayload(value)) {
    throw new Error(`${name} is not a wheel://open payload`);
  }
  return value;
}

/** A `toast://show` fixture, guarded. */
export function toastFixture(name: string): ToastShowPayload {
  const value = fixtureJson(name);
  if (!isToastShowPayload(value)) {
    throw new Error(`${name} is not a toast://show payload`);
  }
  return value;
}

/** A `float://view` fixture, guarded. */
export function floatFixture(name: string): FloatView {
  const value = fixtureJson(name);
  if (!isFloatView(value)) {
    throw new Error(`${name} is not a float://view payload`);
  }
  return value;
}

/** A `SettingsView` fixture, guarded. */
export function settingsFixture(name: string): SettingsView {
  const value = fixtureJson(name);
  if (!isSettingsView(value)) {
    throw new Error(`${name} is not a SettingsView`);
  }
  return value;
}
