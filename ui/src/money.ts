// How amounts are shown: grouped, with at least two fraction digits (more when the figure has more:
// a rate or a quantity is never cut). The figure itself stays the server's string.

import { formatDecimal } from "@ferrum/client";

export function show(value: unknown, precision = 2): string {
  if (value === null || value === undefined || value === "") return "";
  const own = typeof value === "string" ? (value.split(".")[1] ?? "").replace(/0+$/, "").length : 0;
  return formatDecimal(value, Math.max(precision, own));
}
