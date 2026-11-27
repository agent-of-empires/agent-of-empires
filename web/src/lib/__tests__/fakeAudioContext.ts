import { vi } from "vitest";

/** What the fake context did. One object per test file, because soundPlayer keeps its context for the page. */
export const fakeAudio = {
  /** The sound each started source decoded from, with its gain. */
  played: [] as { sound: string; gain: number }[],
  /** Sounds decoded so far: a load that finished, whether or not it then played. */
  decoded: 0,
  state: "running" as AudioContextState,
  /** Whether `resume` starts the context, as after a gesture; otherwise it never settles. */
  resumable: true,
};

/** Bytes `decodeAudioData` turns back into the sound's name. */
export const soundBytes = (name: string): ArrayBuffer => new TextEncoder().encode(name).buffer as ArrayBuffer;

class FakeAudioContext {
  destination = {};
  get state() {
    return fakeAudio.state;
  }
  resume(): Promise<void> {
    if (!fakeAudio.resumable) return new Promise(() => {});
    fakeAudio.state = "running";
    return Promise.resolve();
  }
  decodeAudioData(bytes: ArrayBuffer) {
    fakeAudio.decoded++;
    return Promise.resolve({ sound: new TextDecoder().decode(bytes) });
  }
  createGain() {
    return { gain: { value: 1 }, connect: () => {} };
  }
  createBufferSource() {
    const source = {
      buffer: null as { sound: string } | null,
      gainNode: null as { gain: { value: number } } | null,
      connect(node: { gain: { value: number } }) {
        source.gainNode = node;
      },
      start() {
        fakeAudio.played.push({ sound: source.buffer!.sound, gain: source.gainNode!.gain.value });
      },
    };
    return source;
  }
}

/** Resets the record and stubs `AudioContext`; call in `beforeEach`. */
export function installFakeAudioContext(): void {
  fakeAudio.played = [];
  fakeAudio.decoded = 0;
  fakeAudio.state = "running";
  fakeAudio.resumable = true;
  vi.stubGlobal("AudioContext", FakeAudioContext);
}

/** The names of the sounds played so far. */
export const playedSounds = (): string[] => fakeAudio.played.map((p) => p.sound);
