export type VideoSize = { width: number; height: number };

/** Reads the HEVC sequence header in an Annex B keyframe. */
export function hevcSequence(frame: Uint8Array): (VideoSize & { codec: string; chroma: number }) | null {
  const end = Math.min(frame.length, 8192);
  for (let i = 0; i + 5 < end; i++) {
    if (frame[i] !== 0 || frame[i + 1] !== 0 || frame[i + 2] !== 1 || ((frame[i + 3] >> 1) & 63) !== 33) continue;
    const rbsp: number[] = [];
    let zeros = 0;
    for (let j = i + 5; j < end; j++) {
      const byte = frame[j];
      if (zeros >= 2 && byte === 1) break;
      if (zeros >= 2 && byte === 3) {
        zeros = 0;
        continue;
      }
      rbsp.push(byte);
      zeros = byte === 0 ? zeros + 1 : 0;
    }

    let offset = 0;
    const read = (bits: number) => {
      if (offset + bits > rbsp.length * 8) throw new RangeError("Truncated sequence header");
      let result = 0;
      while (bits-- > 0) {
        result = result * 2 + ((rbsp[offset >> 3] >> (7 - (offset & 7))) & 1);
        offset++;
      }
      return result;
    };
    const skip = (bits: number) => {
      offset += bits;
      if (offset > rbsp.length * 8) throw new RangeError("Truncated sequence header");
    };
    const readUnsigned = () => {
      let leadingZeros = 0;
      while (read(1) === 0) {
        if (++leadingZeros >= 24) throw new RangeError("Invalid sequence header value");
      }
      return 2 ** leadingZeros - 1 + read(leadingZeros);
    };

    try {
      skip(4);
      const layers = read(3);
      skip(1); // Temporal nesting
      const space = ["", "A", "B", "C"][read(2)];
      const tier = read(1) ? "H" : "L";
      const profile = read(5);
      let compatibility = 0;
      for (let bit = 0; bit < 32; bit++) compatibility += read(1) * 2 ** bit;
      const constraints = Array.from({ length: 6 }, () => read(8).toString(16).toUpperCase().padStart(2, "0"));
      while (constraints[constraints.length - 1] === "00") constraints.pop();
      const level = read(8);
      const codec = ["hev1", `${space}${profile}`, compatibility.toString(16).toUpperCase(), `${tier}${level}`, ...constraints].join(".");
      const flags = Array.from({ length: layers }, () => [read(1), read(1)]);
      if (layers > 0) skip((8 - layers) * 2);
      for (const [profile, level] of flags) {
        if (profile) skip(88);
        if (level) skip(8);
      }

      readUnsigned(); // Sequence parameter set ID.
      const chroma = readUnsigned();
      if (chroma > 3) return null;
      if (chroma === 3) skip(1);
      let width = readUnsigned();
      let height = readUnsigned();
      if (read(1)) {
        width -= (readUnsigned() + readUnsigned()) * (chroma === 1 || chroma === 2 ? 2 : 1);
        height -= (readUnsigned() + readUnsigned()) * (chroma === 1 ? 2 : 1);
      }
      return width >= 64 && height >= 64 && width <= 8192 && height <= 8192 ? { width, height, codec, chroma } : null;
    } catch {
      return null;
    }
  }
  return null;
}
