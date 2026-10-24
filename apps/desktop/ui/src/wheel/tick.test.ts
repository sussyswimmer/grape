import { describe, expect, it, vi } from "vitest";
import { createTick, TICK, type TickAudio } from "./tick";

type Call = readonly [string, number, number];

class FakeParam {
  readonly calls: Call[] = [];

  setValueAtTime(value: number, time: number): this {
    this.calls.push(["set", value, time]);
    return this;
  }

  exponentialRampToValueAtTime(value: number, time: number): this {
    this.calls.push(["exp", value, time]);
    return this;
  }
}

class FakeNode {
  readonly connections: unknown[] = [];
  disconnected = 0;

  connect(node: unknown): unknown {
    this.connections.push(node);
    return node;
  }

  disconnect(): void {
    this.disconnected += 1;
  }
}

class FakeOscillator extends FakeNode {
  type = "square";
  readonly frequency = new FakeParam();
  started: number | null = null;
  readonly stops: (number | undefined)[] = [];
  onended: (() => void) | null = null;
  stopThrows = false;

  start(when: number): void {
    this.started = when;
  }

  stop(when?: number): void {
    if (this.stopThrows) throw new Error("InvalidStateError");
    this.stops.push(when);
  }
}

class FakeGain extends FakeNode {
  readonly gain = new FakeParam();
}

class FakeAudio {
  currentTime = 1;
  readonly destination = { name: "speakers" };
  readonly oscillators: FakeOscillator[] = [];
  readonly gains: FakeGain[] = [];
  resumes = 0;
  resumeResult: () => Promise<void> = () => Promise.resolve();

  resume(): Promise<void> {
    this.resumes += 1;
    return this.resumeResult();
  }

  createOscillator(): FakeOscillator {
    const oscillator = new FakeOscillator();
    this.oscillators.push(oscillator);
    return oscillator;
  }

  createGain(): FakeGain {
    const gain = new FakeGain();
    this.gains.push(gain);
    return gain;
  }
}

function setup(make: () => FakeAudio | null = () => new FakeAudio()) {
  const created: FakeAudio[] = [];
  const warn = vi.fn();
  const tick = createTick({
    createAudio: () => {
      const audio = make();
      if (audio !== null) created.push(audio);
      return audio as unknown as TickAudio | null;
    },
    warn,
  });
  const audio = (): FakeAudio => {
    const [first] = created;
    if (first === undefined) throw new Error("no AudioContext yet");
    return first;
  };
  return { tick, created, warn, audio };
}

describe("createTick", () => {
  it("creates no AudioContext until a wheel opens with sounds on, then only one", () => {
    const { tick, created } = setup();
    expect(created).toHaveLength(0);
    tick.opened(false);
    tick.play();
    expect(created).toHaveLength(0);
    tick.opened(true);
    tick.opened(true);
    tick.play();
    expect(created).toHaveLength(1);
  });

  it("resumes the context on every open", () => {
    const { tick, audio } = setup();
    tick.opened(true);
    tick.opened(true);
    expect(audio().resumes).toBe(2);
  });

  it("plays the synthesized tick with the exact envelope", () => {
    const { tick, audio } = setup();
    tick.opened(true);
    audio().currentTime = 2;
    tick.play();
    const [osc] = audio().oscillators;
    const [gain] = audio().gains;
    if (osc === undefined || gain === undefined) throw new Error("no nodes");
    expect(osc.type).toBe("sine");
    expect(osc.frequency.calls).toEqual([
      ["set", 1320, 2],
      ["exp", 990, 2 + 0.025],
    ]);
    expect(gain.gain.calls).toEqual([
      ["set", 0.06, 2],
      ["exp", 0.0001, 2 + 0.035],
    ]);
    expect(osc.started).toBe(2);
    expect(osc.stops).toEqual([2 + 0.04]);
    expect(osc.connections).toEqual([gain]);
    expect(gain.connections).toEqual([audio().destination]);
    osc.onended?.();
    expect(osc.disconnected).toBe(1);
    expect(gain.disconnected).toBe(1);
  });

  it("matches the documented constants", () => {
    expect(TICK).toEqual({
      startHz: 1320,
      endHz: 990,
      glideSeconds: 0.025,
      startGain: 0.06,
      endGain: 0.0001,
      fadeSeconds: 0.035,
      stopSeconds: 0.04,
      cancelWindowSeconds: 0.01,
    });
  });

  it("stays silent when the wheel's sounds setting is off", () => {
    const { tick, audio } = setup();
    tick.opened(true);
    tick.opened(false);
    tick.play();
    expect(audio().oscillators).toHaveLength(0);
  });

  it("cancels a tick that started within the last 10 ms", () => {
    const { tick, audio } = setup();
    tick.opened(true);
    tick.play();
    audio().currentTime += 0.004;
    tick.cancel();
    expect(audio().oscillators[0]?.stops).toEqual([1 + 0.04, undefined]);
  });

  it("lets an older tick finish instead of clipping it", () => {
    const { tick, audio } = setup();
    tick.opened(true);
    tick.play();
    audio().currentTime += 0.02;
    tick.cancel();
    expect(audio().oscillators[0]?.stops).toEqual([1 + 0.04]);
  });

  it("cancels only once, and does nothing with no tick", () => {
    const { tick, audio } = setup();
    tick.cancel();
    tick.opened(true);
    tick.cancel();
    tick.play();
    tick.cancel();
    tick.cancel();
    expect(audio().oscillators[0]?.stops).toHaveLength(2);
  });

  it("never throws when WebAudio is missing, and warns once", () => {
    const { tick, warn } = setup(() => null);
    expect(() => {
      tick.opened(true);
      tick.play();
      tick.cancel();
      tick.opened(true);
      tick.play();
    }).not.toThrow();
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it("never throws when creating the context fails, and warns once", () => {
    const { tick, warn } = setup(() => {
      throw new Error("NotAllowedError");
    });
    expect(() => {
      tick.opened(true);
      tick.play();
      tick.play();
    }).not.toThrow();
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it("warns once when resume is blocked by autoplay policy", async () => {
    const blocked = new FakeAudio();
    blocked.resumeResult = () => Promise.reject(new Error("autoplay blocked"));
    const { tick, warn } = setup(() => blocked);
    tick.opened(true);
    tick.opened(true);
    await Promise.resolve();
    await Promise.resolve();
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it("never throws when scheduling or stopping fails", () => {
    const broken = new FakeAudio();
    const { tick, warn, audio } = setup(() => broken);
    tick.opened(true);
    broken.createOscillator = () => {
      throw new Error("closed");
    };
    expect(() => tick.play()).not.toThrow();
    expect(warn).toHaveBeenCalledTimes(1);

    broken.createOscillator = FakeAudio.prototype.createOscillator.bind(broken);
    tick.play();
    const osc = audio().oscillators[0];
    if (osc === undefined) throw new Error("no oscillator");
    osc.stopThrows = true;
    expect(() => tick.cancel()).not.toThrow();
  });
});
