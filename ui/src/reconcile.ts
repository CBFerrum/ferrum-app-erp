// The bank reconciliation workbench's rules: moving through statement lines, what Enter accepts,
// how several entries are ticked against one line (with amounts that can be edited), and how a
// statement file's columns are guessed. Pure, so the screen's rules are tested without a browser.
// Amounts are the server's decimal strings, added and compared exactly (decimal.ts).

import { cmp, isDecimal, isZero, min, normalize, sub, sum } from "./decimal";
import { t } from "@ferrum/client";

export interface Match {
  payment_document: "PaymentEntry" | "JournalEntry";
  payment_entry: string;
  number?: string | null;
  posting_date: string;
  /** What the entry moves, without sign. */
  amount: string;
  /** What of it no statement line has cleared yet. */
  remaining: string;
  party?: string | null;
  reference?: string | null;
  exact_amount: boolean;
  reference_match: boolean;
  party_match: boolean;
  days_apart: number;
  score: string;
}

export interface Cleared {
  payment_document: string;
  payment_entry: string;
  payment_number?: string | null;
  posting_date?: string | null;
  allocated_amount: string;
}

export interface Line {
  id: string;
  number?: string | null;
  date: string;
  description?: string | null;
  reference_number?: string | null;
  deposit: string;
  withdrawal: string;
  /** Signed: money in is positive. */
  amount: string;
  allocated: string;
  unallocated: string;
  status: "Unreconciled" | "Partially Reconciled" | "Reconciled" | string;
  version: number;
  /** The bank's balance after this line; absent for lines inside the opening balance. */
  balance: string | null;
  allocations: Cleared[];
  /** The entry that clears this line exactly, when there is one. */
  match: Match | null;
}

export interface Book {
  payment_document: "PaymentEntry" | "JournalEntry";
  payment_entry: string;
  number?: string | null;
  posting_date: string;
  party?: string | null;
  reference?: string | null;
  /** Signed: money in is positive. */
  amount: string;
  remaining: string;
}

export interface Figures {
  statement_balance: string;
  book_balance: string;
  difference: string;
  uncleared_deposits: string;
  uncleared_payments: string;
  unrecorded: string;
  expected_bank_balance: string;
  unexplained: string;
}

export interface Workbench {
  bank_account: string;
  name?: string | null;
  currency: string;
  precision: number;
  as_of: string;
  figures: Figures;
  counts: { unreconciled: number; matched: number };
  lines: Line[];
  books: Book[];
  truncated: boolean;
  books_truncated: boolean;
}

export const isDeposit = (line: Pick<Line, "amount">): boolean => cmp(line.amount, "0") > 0;

/** The line a movement key lands on, `delta` lines on (the ends stay put); the first line when
 *  nothing is selected yet. */
export function step(lines: Pick<Line, "id">[], current: string | null, delta: 1 | -1): string | null {
  if (!lines.length) return null;
  const at = lines.findIndex((l) => l.id === current);
  if (at < 0) return lines[delta > 0 ? 0 : lines.length - 1].id;
  return lines[Math.min(lines.length - 1, Math.max(0, at + delta))].id;
}

/** What one key press accepts on a line: its exact match, and only that. */
export function enterTarget(line: Pick<Line, "match"> | undefined): Match | null {
  return line?.match?.exact_amount ? line.match : null;
}

/** The lines "accept all" would settle, and what they come to in each direction. */
export function exactMatches(lines: Line[]): { lines: Line[]; money_in: string; money_out: string } {
  const picked = lines.filter((l) => enterTarget(l));
  return {
    lines: picked,
    money_in: sum(picked.filter(isDeposit).map((l) => l.unallocated)),
    money_out: sum(picked.filter((l) => !isDeposit(l)).map((l) => l.unallocated)),
  };
}

/** Why an entry was put forward, in words. */
export function why(m: Pick<Match, "exact_amount" | "reference_match" | "party_match" | "days_apart">): string[] {
  const out: string[] = [];
  if (m.exact_amount) out.push("Exact amount");
  if (m.reference_match) out.push("Same reference");
  if (m.party_match) out.push("Party named");
  out.push(m.days_apart === 0 ? "Same day" : m.days_apart === 1 ? "1 day apart" : `${m.days_apart} days apart`);
  return out;
}

// ── ticking several entries against one line ──

/** The entries ticked, each with the amount of the line it clears (as typed). */
export type Ticks = Record<string, string>;

/** What of the line is not yet spoken for. */
export function leftToReconcile(unallocated: string, ticks: Ticks): string {
  return sub(unallocated, sum(Object.values(ticks)));
}

/** Ticks an entry for as much as it can take of what is left of the line. */
export function tick(ticks: Ticks, entry: { id: string; remaining: string }, unallocated: string): Ticks {
  const left = leftToReconcile(unallocated, ticks);
  return { ...ticks, [entry.id]: cmp(left, "0") > 0 ? min(entry.remaining, left) : "0" };
}

export function untick(ticks: Ticks, id: string): Ticks {
  const { [id]: _gone, ...rest } = ticks;
  return rest;
}

/** What is wrong with the amounts, per entry, and with their total; empty when they can go. */
export function problems(ticks: Ticks, entries: { id: string; remaining: string }[], unallocated: string): { amounts: Record<string, string>; total: string | null } {
  const amounts: Record<string, string> = {};
  for (const [id, text] of Object.entries(ticks)) {
    const entry = entries.find((e) => e.id === id);
    if (!isDecimal(text) || cmp(text, "0") <= 0) amounts[id] = "Enter an amount above zero";
    else if (entry && cmp(text, entry.remaining) > 0) amounts[id] = `Only ${normalize(entry.remaining)} of this is left to clear`;
  }
  const over = cmp(sum(Object.values(ticks)), unallocated) > 0;
  return { amounts, total: over ? `That is more than the ${normalize(unallocated)} left on the line` : null };
}

export const canReconcile = (ticks: Ticks, p: ReturnType<typeof problems>): boolean => Object.keys(ticks).length > 0 && !p.total && !Object.keys(p.amounts).length;

/** The `reconcile` action's body for the ticked entries. */
export function reconcileBody(ticks: Ticks, entries: { id: string; kind: string }[]): { vouchers: { payment_document: string; payment_entry: string; allocated_amount: string }[] } {
  return {
    vouchers: Object.entries(ticks).map(([id, amount]) => ({
      payment_document: entries.find((e) => e.id === id)?.kind ?? "PaymentEntry",
      payment_entry: id,
      allocated_amount: normalize(amount),
    })),
  };
}

/** Whether ticking the entries clears the line in full. */
export const clearsLine = (unallocated: string, ticks: Ticks): boolean => isZero(leftToReconcile(unallocated, ticks)) && Object.keys(ticks).length > 0;

/** The books' entries that could clear a line: the right direction, and not already put forward. */
export function others(books: Book[], line: Pick<Line, "amount">, put: Set<string>): Book[] {
  const deposit = isDeposit(line);
  return books.filter((b) => !put.has(b.payment_entry) && (cmp(b.amount, "0") > 0) === deposit);
}

// ── reading a statement file ──

const DELIMITERS = [",", ";", "\t", "|"] as const;

/** The delimiter of a line: the commonest outside quotes. */
export function sniffDelimiter(line: string): string {
  const counts = new Map<string, number>(DELIMITERS.map((d) => [d, 0]));
  let quoted = false;
  for (const c of line) {
    if (c === '"') quoted = !quoted;
    else if (!quoted && counts.has(c)) counts.set(c, (counts.get(c) ?? 0) + 1);
  }
  return DELIMITERS.reduce((best, d) => ((counts.get(d) ?? 0) > (counts.get(best) ?? 0) ? d : best), DELIMITERS[0]);
}

/** The statement's header row: its column names and the delimiter between them (which is sent to
 *  the server too, so a file with lines above its header is read the same way on both sides). */
export function csvHeaders(text: string, headerRow = 1): { headers: string[]; delimiter: string } {
  const line = text.split(/\r?\n/).filter((l) => l.trim())[headerRow - 1] ?? "";
  const delimiter = sniffDelimiter(line);
  const headers: string[] = [];
  let cell = "";
  let quoted = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === '"') {
      if (quoted && line[i + 1] === '"') {
        cell += '"';
        i++;
      } else quoted = !quoted;
    } else if (c === delimiter && !quoted) {
      headers.push(cell.trim());
      cell = "";
    } else cell += c;
  }
  headers.push(cell.trim());
  return { headers, delimiter };
}

export interface Mapping {
  date: string;
  description: string;
  reference: string;
  amount: string;
  deposit: string;
  withdrawal: string;
}

const GUESSES: [keyof Mapping, RegExp][] = [
  ["date", /^(txn|transaction|value|posting|booking)?\s*date$|^datum$|^fecha$/i],
  ["deposit", /deposit|credit|money in|paid in|received|^cr$/i],
  ["withdrawal", /withdraw|debit|money out|paid out|^dr$/i],
  ["amount", /^(net |txn |transaction )?amount$|^amt$|^value$/i],
  ["description", /desc|narration|details|particulars|memo|payee|^text$|^name$/i],
  ["reference", /ref|cheque|check no|utr|trans(action)? id|^id$/i],
];

/** Which column is which, by the names banks give them; blank where nothing fits. */
export function guessMapping(headers: string[]): Mapping {
  const m: Mapping = { date: "", description: "", reference: "", amount: "", deposit: "", withdrawal: "" };
  const taken = new Set<string>();
  for (const [field, pattern] of GUESSES) {
    const found = headers.find((h) => h && !taken.has(h) && pattern.test(h.trim()));
    if (found) {
      m[field] = found;
      taken.add(found);
    }
  }
  // One signed amount column, or a column each for money in and out: not both.
  if (m.amount && (m.deposit || m.withdrawal)) m.amount = "";
  return m;
}

export function mappingProblem(m: Mapping): string | null {
  if (!m.date) return "Choose the column that holds the date.";
  if (!m.amount && !m.deposit && !m.withdrawal) return "Choose the amount column, or one column for money in and one for money out.";
  return null;
}

/** The `mapping` the server takes: only the columns chosen. */
export function mappingBody(m: Mapping): Record<string, string> {
  return Object.fromEntries(Object.entries(m).filter(([, v]) => v));
}

/** How the statement writes its dates, as the server's `date_format` (empty: day first, or ISO). */
export const DATE_FORMATS: { label: string; value: string }[] = [
  { get label() { return t("Day first (31/12/2026) or 2026-12-31"); }, value: "" },
  { get label() { return t("Month first (12/31/2026)"); }, value: "%m/%d/%Y" },
  { get label() { return t("Year first (2026/12/31)"); }, value: "%Y/%m/%d" },
];
