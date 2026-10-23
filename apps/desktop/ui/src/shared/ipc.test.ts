import { describe, expect, it, vi } from "vitest";
import { callQuietly, type InvokeFn } from "./ipc";

describe("callQuietly", () => {
  it("passes the command and args through", () => {
    const invoke = vi.fn(() => Promise.resolve());
    callQuietly(invoke, "wheel_painted", { seq: 3 });
    expect(invoke).toHaveBeenCalledWith("wheel_painted", { seq: 3 });
  });

  it("swallows rejections and synchronous throws", async () => {
    const rejecting: InvokeFn = () => Promise.reject(new Error("no ipc"));
    const throwing: InvokeFn = () => {
      throw new Error("no ipc");
    };
    expect(() => callQuietly(rejecting, "toast_clicked", { id: 1 })).not.toThrow();
    expect(() => callQuietly(throwing, "toast_clicked", { id: 1 })).not.toThrow();
    await Promise.resolve();
  });
});
