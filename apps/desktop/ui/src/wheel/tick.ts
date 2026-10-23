/**
 * The highlight tick (docs/design/m1-drag-wheel.md §D.3): Grape's own
 * short sound, synthesized with WebAudio, so there is no audio file.
 *
 * - One AudioContext, created lazily on the first open with sounds on and
 *   resumed on every open (the overlay never gets a user gesture; Rust
 *   starts WebView2 with an autoplay flag so `resume()` is allowed).
 * - A tick is a sine that glides 1320 Hz → 990 Hz over 25 ms while its
 *   gain falls 0.06 → 0.0001 over 35 ms, stopped at 40 ms.
 * - Cancel stops a tick that started less than 10 ms ago (or hasn't
 *   started yet), so skimming across a gap stays quiet.
 * - It never throws. If audio is blocked or missing it warns once and
 *   stays silent.
 */

/** Plays or cancels the highlight tick. The wheel view drives it. */
export interface SoundSink {
  /** A wheel opened or swapped; `sounds` is the payload's setting. */
  opened(sounds: boolean): void;
  /** The highlight moved to a new segment. */
  play(): void;
  /** The highlight left every segment (or the wheel closed). */
  cancel(): void;
}

/** The parts of `AudioContext` the tick uses; a real `AudioContext` fits. */
export interface TickAudio {
  readonly currentTime: number;
  readonly destination: AudioNode;
  resume(): Promise<void>;
  createOscillator(): OscillatorNode;
  createGain(): GainNode;
}

/** The tick's sound, in Hz, seconds and linear gain. */
export const TICK = {
  startHz: 1320,
  endHz: 990,
  glideSeconds: 0.025,
  startGain: 0.06,
  endGain: 0.0001,
  fadeSeconds: 0.035,
  stopSeconds: 0.04,
  /** A cancel stops a tick that started less than this long ago. */
  cancelWindowSeconds: 0.01,
} as const;

export interface TickOptions {
  /** Makes the one AudioContext. Defaults to `new AudioContext()` when available. */
  readonly createAudio?: () => TickAudio | null;
  /** Where the one warning goes. Defaults to `console.warn`. */
  readonly warn?: (message: string, error: unknown) => void;
}

function defaultAudio(): TickAudio | null {
  return typeof AudioContext === "function" ? new AudioContext() : null;
}

/** Makes the tick. Nothing touches WebAudio until a wheel opens with sounds on. */
export function createTick(options: TickOptions = {}): SoundSink {
  const createAudio = options.createAudio ?? defaultAudio;
  const warn = options.warn ?? ((message: string, error: unknown) => console.warn(message, error));

  let audio: TickAudio | null = null;
  let unavailable = false;
  let warned = false;
  let enabled = false;
  let last: { oscillator: OscillatorNode; startAt: number } | null = null;

  function warnOnce(error: unknown): void {
    if (!warned) {
      warned = true;
      warn("Grape: the highlight tick is unavailable, so the wheel stays silent.", error);
    }
  }

  function context(): TickAudio | null {
    if (audio === null && !unavailable) {
      try {
        audio = createAudio();
        if (audio === null) {
          unavailable = true;
          warnOnce(new Error("WebAudio is not available"));
        }
      } catch (error: unknown) {
        unavailable = true;
        warnOnce(error);
      }
    }
    return audio;
  }

  return {
    opened(sounds) {
      enabled = sounds;
      if (!sounds) {
        return;
      }
      const ctx = context();
      if (ctx === null) {
        return;
      }
      try {
        ctx.resume().catch(warnOnce);
      } catch (error: unknown) {
        warnOnce(error);
      }
    },

    play() {
      if (!enabled) {
        return;
      }
      const ctx = context();
      if (ctx === null) {
        return;
      }
      try {
        const t0 = ctx.currentTime;
        const oscillator = ctx.createOscillator();
        const gain = ctx.createGain();
        oscillator.type = "sine";
        oscillator.frequency.setValueAtTime(TICK.startHz, t0);
        oscillator.frequency.exponentialRampToValueAtTime(TICK.endHz, t0 + TICK.glideSeconds);
        gain.gain.setValueAtTime(TICK.startGain, t0);
        gain.gain.exponentialRampToValueAtTime(TICK.endGain, t0 + TICK.fadeSeconds);
        oscillator.connect(gain);
        gain.connect(ctx.destination);
        oscillator.onended = () => {
          try {
            oscillator.disconnect();
            gain.disconnect();
          } catch {
            // Already disconnected.
          }
        };
        oscillator.start(t0);
        oscillator.stop(t0 + TICK.stopSeconds);
        last = { oscillator, startAt: t0 };
      } catch (error: unknown) {
        warnOnce(error);
      }
    },

    cancel() {
      const tick = last;
      last = null;
      if (tick === null || audio === null) {
        return;
      }
      try {
        if (tick.startAt > audio.currentTime - TICK.cancelWindowSeconds) {
          tick.oscillator.stop();
        }
      } catch {
        // Already stopped; nothing to cancel.
      }
    },
  };
}
