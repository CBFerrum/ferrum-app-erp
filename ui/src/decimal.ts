// Exact arithmetic on the decimal strings the server sends. Money in these screens never goes
// through a float: 0.1 + 0.2 is 0.3 here, and an allocation that adds up on screen adds up on the
// server. Values are whole numbers of 10^-scale (BigInt), so nothing is rounded until a figure is
// shown (`formatDecimal` in @ferrum/client).

const DECIMAL = /^([-+]?)(\d+)(?:\.(\d+))?$/;

interface Parsed {
  units: bigint;
  scale: number;
}

function parse(value: string): Parsed | null {
  const m = DECIMAL.exec(value.trim());
  if (!m) return null;
  const frac = m[3] ?? "";
  const units = BigInt(m[2] + frac);
  return { units: m[1] === "-" ? -units : units, scale: frac.length };
}

/** `value` as the server would write it: no trailing zeros, no "-0". */
function write({ units, scale }: Parsed): string {
  const negative = units < 0n;
  const digits = (negative ? -units : units).toString().padStart(scale + 1, "0");
  const int = digits.slice(0, digits.length - scale);
  const frac = digits.slice(digits.length - scale).replace(/0+$/, "");
  const text = frac ? `${int}.${frac}` : int;
  return negative && text !== "0" ? `-${text}` : text;
}

/** Both numbers over the same power of ten. */
function align(a: Parsed, b: Parsed): [bigint, bigint, number] {
  const scale = Math.max(a.scale, b.scale);
  return [a.units * 10n ** BigInt(scale - a.scale), b.units * 10n ** BigInt(scale - b.scale), scale];
}

/** Whether `value` is a decimal this module can work with. */
export function isDecimal(value: unknown): value is string {
  return typeof value === "string" && parse(value) !== null;
}

/** An empty or unreadable value counts as nothing: a blank box is zero, not an error. */
function read(value: string | null | undefined): Parsed {
  return (value ? parse(value) : null) ?? { units: 0n, scale: 0 };
}

export function add(a: string | null | undefined, b: string | null | undefined): string {
  const [x, y, scale] = align(read(a), read(b));
  return write({ units: x + y, scale });
}

export function sub(a: string | null | undefined, b: string | null | undefined): string {
  const [x, y, scale] = align(read(a), read(b));
  return write({ units: x - y, scale });
}

export function neg(a: string | null | undefined): string {
  const p = read(a);
  return write({ units: -p.units, scale: p.scale });
}

export function abs(a: string | null | undefined): string {
  const p = read(a);
  return write({ units: p.units < 0n ? -p.units : p.units, scale: p.scale });
}

export function sum(values: (string | null | undefined)[]): string {
  return values.reduce<string>((total, v) => add(total, v), "0");
}

/** -1, 0 or 1. */
export function cmp(a: string | null | undefined, b: string | null | undefined): -1 | 0 | 1 {
  const [x, y] = align(read(a), read(b));
  return x < y ? -1 : x > y ? 1 : 0;
}

export const sign = (a: string | null | undefined): -1 | 0 | 1 => cmp(a, "0");
export const isZero = (a: string | null | undefined): boolean => sign(a) === 0;

export function min(a: string, b: string): string {
  return cmp(a, b) <= 0 ? normalize(a) : normalize(b);
}

export function max(a: string, b: string): string {
  return cmp(a, b) >= 0 ? normalize(a) : normalize(b);
}

/** The canonical spelling of a decimal ("1500.50" is "1500.5"; unreadable is "0"). */
export function normalize(a: string | null | undefined): string {
  return write(read(a));
}

/** The fraction digits a decimal is written with. */
export function places(a: string | null | undefined): number {
  return read(a).scale;
}
