// What the screens work out themselves: a run's words, and what a figure can honestly be called.

import type { Tone } from "@ferrum/kit";

export function runWords(status: string): { label: string; tone: Tone } {
  switch (status) {
    case "Planned":
      return { label: "Planned", tone: "neutral" };
    case "Released":
      return { label: "Ready to start", tone: "blue" };
    case "Running":
      return { label: "Running", tone: "green" };
    case "Paused":
      return { label: "Stopped", tone: "amber" };
    case "Completed":
      return { label: "Finished", tone: "neutral" };
    case "Cancelled":
      return { label: "Cancelled", tone: "neutral" };
    default:
      return { label: status, tone: "neutral" };
  }
}

export const percent = (v: number | null | undefined) => (v === null || v === undefined ? "Unknown" : `${(v * 100).toFixed(1)}%`);

/** A figure from a period partly unseen says so. Under a tenth missing it would only be noise. */
export function coverageNote(coverage: number | null | undefined): string | null {
  if (coverage === null || coverage === undefined) return "Nothing was planned in this period.";
  if (coverage >= 0.9) return null;
  return `These figures cover ${Math.round(coverage * 100)}% of the period. Nothing is known about the rest, and it is not counted as running.`;
}

/** Performance over 100% means the ideal cycle time is wrong, which someone should look at. */
export const cycleWarning = (performance: number | null | undefined) => (performance !== null && performance !== undefined && performance > 1.02 ? "Running faster than its ideal cycle: that figure on the revision is probably wrong." : null);

/** What is left to report on a run. */
export function remaining(qty: string, good?: string | null, scrap?: string | null) {
  return Math.max(Number(qty) - Number(good ?? 0) - Number(scrap ?? 0), 0);
}
