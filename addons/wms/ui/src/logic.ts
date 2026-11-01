// What the screens work out for themselves. The server holds every rule: this only says what a task
// is in words and which task comes next, so that it can be tested without a browser.

import type { Tone } from "@ferrum/kit";

export interface Task {
  id: string;
  summary: string;
  kind: string;
  status: string;
  custody: string | null;
  qty: string;
  reason: string | null;
  item: string;
  lot: string | null;
  from_bin: string | null;
  to_bin: string | null;
  robot: string | null;
}

/** Whose move it is, and what to tell them. `Arrived` is the one people get wrong: the robot is there
 *  and the stock has not moved. */
export function taskWords(status: string): { label: string; tone: Tone; says: string } {
  switch (status) {
    case "Open":
      return { label: "To do", tone: "neutral", says: "Nobody has this yet." };
    case "Released":
      return { label: "Released", tone: "blue", says: "Ready for whoever takes it." };
    case "Moving":
      return { label: "On its way", tone: "blue", says: "A robot has it. The stock stays where it was until someone confirms it." };
    case "Arrived":
      return { label: "Arrived", tone: "amber", says: "The robot says it is there. Scan the destination: nothing has moved until you do." };
    case "Exception":
      return { label: "Needs someone", tone: "red", says: "Nobody can say where this is. Find it, then say where." };
    case "Short":
      return { label: "Short", tone: "amber", says: "Less moved than was asked." };
    case "Done":
      return { label: "Done", tone: "green", says: "Moved and confirmed." };
    default:
      return { label: status, tone: "neutral", says: "" };
  }
}

export function custodyWords(custody: string | null): string | null {
  if (custody === "In Transit") return "On its way, and still counted at its source";
  if (custody === "Uncertain") return "Nobody can say where this is. It stays out of reach until someone finds it";
  return null;
}

/** Tasks in the order someone should act: what is stuck first, then what is waiting for a scan. */
export function byUrgency(tasks: Task[]): Task[] {
  const rank = (t: Task) => ["Exception", "Arrived", "Released", "Open", "Moving"].indexOf(t.status);
  return [...tasks].sort((a, b) => rank(a) - rank(b));
}

/** Any scanner may add spaces or a newline, and bins are labelled in capitals. */
export const cleanScan = (raw: string) => raw.trim();

export function shortOf(qty: string, moved: string): boolean {
  return Number(moved) < Number(qty);
}

export interface ReceiveLine {
  id: string;
  item: string;
  expected_qty: string;
  received_qty?: string | null;
  damaged_qty?: string | null;
  lot?: string | null;
  _titles?: Record<string, string>;
}

/** How far a line has come. Over what was expected is shown, not hidden: a manager accepts it. */
export function lineProgress(l: ReceiveLine): { got: number; left: number; over: boolean } {
  const got = Number(l.received_qty ?? 0) + Number(l.damaged_qty ?? 0);
  const expected = Number(l.expected_qty);
  return { got, left: Math.max(expected - got, 0), over: got > expected };
}

export interface ShipLine {
  id: string;
  item: string;
  qty: string;
  verified_qty?: string | null;
  lot?: string | null;
  _titles?: Record<string, string>;
}

export const unverified = (lines: ShipLine[]) => lines.filter((l) => Number(l.verified_qty ?? 0) < Number(l.qty));
