// What the screens work out themselves: a run's words, and what a figure can honestly be called.

import type { Tone } from "@ferrum/kit";

/** Whether a released run has what it needs at the station. Only then does the machine get its job. */
export const materialThere = (status: string | null | undefined) => status === "At Station" || status === "Preloaded";

export function materialWords(status: string | null | undefined): string | null {
  switch (status) {
    case "Waiting":
      return "Waiting for the warehouse to take this on. The machine stays off until the material is at the station.";
    case "Reserved":
      return "The warehouse is bringing the material. The machine stays off until it is at the station.";
    case "Short":
      return "Not all of the material can be used. The machine stays off until it can.";
    default:
      return null;
  }
}

export function runWords(status: string, material?: string | null): { label: string; tone: Tone } {
  switch (status) {
    case "Planned":
      return { label: "Planned", tone: "neutral" };
    case "Released":
      if (material !== undefined && !materialThere(material)) return { label: material === "Short" ? "Material short" : "Waiting for material", tone: "amber" };
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
