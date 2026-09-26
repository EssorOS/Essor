/**
 * Fractional order keys compatible with the Rust `fractional_index` crate the
 * editor uses (`src/page.rs::next_position` for blocks and
 * `src/library.rs::next_page_position` for pages).
 *
 * Unlike the JS `fractional-indexing` package, that crate encodes a key as a
 * lower-case hex string over a byte sequence terminated by `0x80`. So the first
 * key is `"80"`, the next `"8180"`, then `"8280"`, and so on. Positions written
 * by the editor are in this format, and the MCP tools must read and write them
 * rather than use a different ordering scheme.
 *
 * `new_after`, `new_before`, and `between` are all implemented: agents append
 * pages and blocks, and `insert_block` can place a block between two neighbors.
 */

/** Byte that terminates every encoded key. */
const TERMINATOR = 0x80;

const HEX_DIGITS = '0123456789abcdef';

/** The smallest key, matching `FractionalIndex::default()`. */
export function defaultKey(): string {
  return byteToHex(TERMINATOR);
}

/**
 * Decode a key into its terminated byte sequence, or `null` when it is not a
 * well-formed key. Matches `FractionalIndex::from_string`: lower-case hex only,
 * a whole number of bytes, and a trailing `0x80` terminator.
 */
export function parseKey(key: string): number[] | null {
  if (key.length === 0 || key.length % 2 !== 0) {
    return null;
  }
  const bytes: number[] = [];
  for (let i = 0; i < key.length; i += 2) {
    const high = hexValue(key[i]);
    const low = hexValue(key[i + 1]);
    if (high < 0 || low < 0) {
      return null;
    }
    bytes.push((high << 4) | low);
  }
  return bytes[bytes.length - 1] === TERMINATOR ? bytes : null;
}

/**
 * A key that sorts after `key`, matching `FractionalIndex::new_after`. A
 * malformed key falls back to the default so the caller never throws on a
 * corrupt catalog entry — the same tolerance `Library::next_position` has.
 */
export function newAfter(key: string): string {
  const bytes = parseKey(key);
  if (bytes === null) {
    return defaultKey();
  }
  return encode([...newAfterBytes(bytes), TERMINATOR]);
}

/**
 * A key that sorts before `key`, matching `FractionalIndex::new_before`. A
 * malformed key falls back to the default, mirroring [`newAfter`].
 */
export function newBefore(key: string): string {
  const bytes = parseKey(key);
  if (bytes === null) {
    return defaultKey();
  }
  return encode([...newBeforeBytes(bytes), TERMINATOR]);
}

/**
 * A key strictly between `lower` and `upper`, matching `FractionalIndex::new`.
 * Either bound may be absent (an open end), in which case the key is placed
 * after the lower or before the upper. Returns `null` when both bounds are
 * present but not in a usable order, so the caller can fall back exactly like
 * `Library::next_position`.
 */
export function between(lower: string | null, upper: string | null): string | null {
  const left = lower === null ? null : parseKey(lower);
  const right = upper === null ? null : parseKey(upper);
  if (left !== null && right !== null) {
    return newBetweenBytes(left, right);
  }
  if (left !== null) {
    return encode([...newAfterBytes(left), TERMINATOR]);
  }
  if (right !== null) {
    return encode([...newBeforeBytes(right), TERMINATOR]);
  }
  return defaultKey();
}

/** Raw byte form of `new_after` (no terminator appended). */
function newAfterBytes(bytes: number[]): number[] {
  for (let i = 0; i < bytes.length; i++) {
    if (bytes[i] < TERMINATOR) {
      return bytes.slice(0, i);
    }
    if (bytes[i] < 0xff) {
      const next = bytes.slice(0, i + 1);
      next[i] += 1;
      return next;
    }
  }
  // A terminated key always has a byte below 0xff, so this is unreachable.
  return [...bytes];
}

/** Raw byte form of `new_before` (no terminator appended). */
function newBeforeBytes(bytes: number[]): number[] {
  for (let i = 0; i < bytes.length; i++) {
    if (bytes[i] > TERMINATOR) {
      return bytes.slice(0, i);
    }
    if (bytes[i] > 0) {
      const next = bytes.slice(0, i + 1);
      next[i] -= 1;
      return next;
    }
  }
  // A terminated key always has a byte above 0, so this is unreachable.
  return [...bytes];
}

/**
 * Encoded form of `FractionalIndex::new_between`, byte-for-byte with the Rust
 * crate so the server and editor agree on the key that lands between two
 * neighbors. Both inputs are terminated byte sequences; the result is an
 * encoded key, or `null` when the bounds are not distinct and in order.
 */
function newBetweenBytes(left: number[], right: number[]): string | null {
  const shorter = Math.min(left.length, right.length) - 1;
  for (let i = 0; i < shorter; i++) {
    if (left[i] < right[i] - 1) {
      const bytes = left.slice(0, i + 1);
      bytes[i] += Math.floor((right[i] - left[i]) / 2);
      return encode([...bytes, TERMINATOR]);
    }
    if (left[i] === right[i] - 1) {
      const prefix = left.slice(0, i + 1);
      const suffix = left.slice(i + 1);
      return encode([...prefix, ...newAfterBytes(suffix), TERMINATOR]);
    }
    if (left[i] > right[i]) {
      return null;
    }
  }
  if (left.length < right.length) {
    const prefix = right.slice(0, shorter + 1);
    const suffix = right.slice(shorter + 1);
    if (prefix[prefix.length - 1] < TERMINATOR) {
      return null;
    }
    return encode([...prefix, ...newBeforeBytes(suffix), TERMINATOR]);
  }
  if (left.length > right.length) {
    const prefix = left.slice(0, shorter + 1);
    const suffix = left.slice(shorter + 1);
    if (prefix[prefix.length - 1] >= TERMINATOR) {
      return null;
    }
    return encode([...prefix, ...newAfterBytes(suffix), TERMINATOR]);
  }
  return null;
}

/**
 * Lexicographic order of two keys, matching the derived `Ord` on the Rust
 * type's terminated byte vector. Malformed keys sort before valid ones.
 */
export function compareKeys(a: string, b: string): number {
  const left = parseKey(a);
  const right = parseKey(b);
  if (left === null || right === null) {
    if (left === null && right === null) {
      return a < b ? -1 : a > b ? 1 : 0;
    }
    return left === null ? -1 : 1;
  }
  const shared = Math.min(left.length, right.length);
  for (let i = 0; i < shared; i++) {
    if (left[i] !== right[i]) {
      return left[i] - right[i];
    }
  }
  return left.length - right.length;
}

function byteToHex(byte: number): string {
  return HEX_DIGITS[byte >> 4] + HEX_DIGITS[byte & 0x0f];
}

function encode(bytes: number[]): string {
  let out = '';
  for (const byte of bytes) {
    out += byteToHex(byte);
  }
  return out;
}

function hexValue(char: string): number {
  if (char >= '0' && char <= '9') {
    return char.charCodeAt(0) - 48;
  }
  if (char >= 'a' && char <= 'f') {
    return char.charCodeAt(0) - 97 + 10;
  }
  return -1;
}
