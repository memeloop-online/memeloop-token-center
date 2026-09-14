/** Decimal conversion never multiplies a binary floating point currency value. */
export function exactMicros(value: string): number | undefined {
  if (value.length > 32 || !/^\d+(?:\.\d{1,6})?$/.test(value)) return undefined;
  const [whole, fraction = ''] = value.split('.');
  const micros = BigInt(whole) * 1_000_000n + BigInt(fraction.padEnd(6, '0'));
  return micros <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(micros) : undefined;
}
