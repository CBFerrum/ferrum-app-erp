import { t } from "@ferrum/client";
// The financial statements' rules: the periods a person picks by name, what a figure opens when
// it is clicked, and the file a statement downloads as. Pure, so they are tested without a browser.
// Dates are `YYYY-MM-DD` text throughout (no time zone can move a day), and amounts are the
// server's decimal strings, never numbers.

export type Kind = "profit_and_loss" | "balance_sheet" | "cash_flow";

export const KINDS: { id: Kind; label: string; path: string }[] = [
  { id: "profit_and_loss", get label() { return t("Profit and loss"); }, path: "/r/profit_and_loss" },
  { id: "balance_sheet", get label() { return t("Balance sheet"); }, path: "/r/balance_sheet" },
  { id: "cash_flow", get label() { return t("Cash flow"); }, path: "/r/cash_flow" },
];

export interface Column {
  name: string;
  label: string;
  type?: string;
}

export interface Row {
  account: string;
  account_id?: string;
  section?: string;
  _indent?: number;
  _bold?: boolean;
  [key: string]: unknown;
}

export interface PeriodInfo {
  key: string;
  label: string;
  from_date: string;
  to_date: string;
  [figure: string]: unknown;
}

export interface Statement {
  columns: Column[];
  rows: Row[];
  totals?: Record<string, string>;
  meta: { company?: string; currency?: string; periods: PeriodInfo[]; balanced?: boolean; reconciled?: boolean; difference?: string };
}

// ── dates ──

const pad = (n: number) => String(n).padStart(2, "0");
const iso = (y: number, m: number, d: number) => `${y}-${pad(m)}-${pad(d)}`;
const parts = (day: string): [number, number, number] => {
  const [y, m, d] = day.split("-").map(Number);
  return [y, m, d];
};

export const monthEnd = (y: number, m: number) => new Date(Date.UTC(y, m, 0)).getUTCDate();

/** `day` moved by whole months, on the first of that month. */
function monthStart(day: string, by: number): [number, number] {
  const [y, m] = parts(day);
  const total = y * 12 + (m - 1) + by;
  return [Math.floor(total / 12), (total % 12) + 1];
}

/** The first day of the fiscal year that `day` is in, when years start in `startMonth`. */
export function fiscalYearStart(day: string, startMonth: number): string {
  const [y, m] = parts(day);
  return iso(m >= startMonth ? y : y - 1, startMonth, 1);
}

function lastDayOfMonths(day: string, months: number): string {
  const [y, m] = monthStart(day, months - 1);
  return iso(y, m, monthEnd(y, m));
}

export interface Preset {
  id: string;
  label: string;
  from: string;
  to: string;
}

/** The periods people ask for by name, for statements of a stretch of time. Years begin in `startMonth`,
 *  and quarters count from the first month of the year. */
export function presets(today: string, startMonth: number): Preset[] {
  const [y, m] = parts(today);
  const thisMonth = iso(y, m, 1);
  const [py, pm] = monthStart(thisMonth, -1);
  const lastMonth = iso(py, pm, 1);
  const fy = fiscalYearStart(today, startMonth);
  const lastFy = iso(parts(fy)[0] - 1, startMonth, 1);
  const quarter = Math.floor((((m - startMonth) % 12) + 12) % 12 / 3);
  const [qy, qm] = monthStart(fy, quarter * 3);
  const thisQuarter = iso(qy, qm, 1);
  const [lqy, lqm] = monthStart(thisQuarter, -3);
  const lastQuarter = iso(lqy, lqm, 1);
  return [
    { id: "this-month", label: t("This month"), from: thisMonth, to: lastDayOfMonths(thisMonth, 1) },
    { id: "last-month", label: t("Last month"), from: lastMonth, to: lastDayOfMonths(lastMonth, 1) },
    { id: "this-quarter", label: t("This quarter"), from: thisQuarter, to: lastDayOfMonths(thisQuarter, 3) },
    { id: "last-quarter", label: t("Last quarter"), from: lastQuarter, to: lastDayOfMonths(lastQuarter, 3) },
    { id: "this-year", label: t("This fiscal year"), from: fy, to: lastDayOfMonths(fy, 12) },
    { id: "last-year", label: t("Last fiscal year"), from: lastFy, to: lastDayOfMonths(lastFy, 12) },
  ];
}

/** The days a balance sheet is usually taken at. */
export function days(today: string, startMonth: number): Preset[] {
  const all = presets(today, startMonth);
  const end = (id: string) => all.find((p) => p.id === id)!.to;
  return [
    { id: "today", label: t("Today"), from: today, to: today },
    { id: "last-month", label: t("End of last month"), from: end("last-month"), to: end("last-month") },
    { id: "last-quarter", label: t("End of last quarter"), from: end("last-quarter"), to: end("last-quarter") },
    { id: "last-year", label: t("End of last fiscal year"), from: end("last-year"), to: end("last-year") },
  ];
}

/** The preset that is exactly this stretch, if any. */
export function named(list: Preset[], from: string, to: string): string {
  return list.find((p) => p.from === from && p.to === to)?.id ?? "custom";
}

// ── what a figure opens ──

/** The columns that hold an amount for a period, in the order of the periods. */
export const amountColumns = (columns: Column[]): Column[] => columns.filter((c) => c.name === "amount" || /^amount_\d+$/.test(c.name));

export interface Drill {
  company: string;
  account: string;
  from: string;
  to: string;
  /** Leave out the entries that close a year: an income statement does. */
  excludeClosing: boolean;
  costCenter?: string | null;
  project?: string | null;
}

/** The general ledger's parameters for the entries behind one figure. */
export function ledgerQuery(d: Drill): Record<string, string> {
  const q: Record<string, string> = { company: d.company, account: d.account, from_date: d.from, to_date: d.to };
  if (d.excludeClosing) q.exclude_closing = "true";
  if (d.costCenter) q.cost_center = d.costCenter;
  if (d.project) q.project = d.project;
  return q;
}

/** The same, as the address of the general ledger report, run. */
export function ledgerHref(d: Drill): string {
  return `/r/general_ledger?${new URLSearchParams({ ...ledgerQuery(d), run: "1" })}`;
}

/** The period a statement's amount column stands for. For a balance sheet, from the year it falls in to the day. */
export function periodOf(kind: Kind, info: PeriodInfo, startMonth: number): { from: string; to: string } {
  return kind === "balance_sheet" ? { from: fiscalYearStart(info.to_date, startMonth), to: info.to_date } : { from: info.from_date, to: info.to_date };
}

// ── the numbers beside the figures ──

/** "−2,000.00 (−66.67%)": how a figure moved, with its sign always shown. */
export function moved(change: string | null | undefined, pct: string | null | undefined, show: (v: string) => string): string {
  if (change === null || change === undefined || change === "") return "";
  const sign = change.startsWith("-") ? "" : change === "0" ? "" : "+";
  const share = pct === null || pct === undefined || pct === "" ? "" : ` (${pct.startsWith("-") || pct === "0" ? "" : "+"}${show(pct)}%)`;
  return `${sign}${show(change)}${share}`;
}

// ── the file ──

const NUMBER = /^[-+]?\d+(\.\d+)?$/;

function cell(value: unknown): string {
  if (value === null || value === undefined) return "";
  const text = String(value);
  // A cell that starts like a formula is text to a spreadsheet, not an instruction.
  const safe = /^[=+\-@\t\r]/.test(text) && !NUMBER.test(text) ? `'${text}` : text;
  return /[",\n\r]/.test(safe) ? `"${safe.replace(/"/g, '""')}"` : safe;
}

/** The statement as CSV, amounts exactly as the server wrote them. */
export function toCsv(s: Pick<Statement, "columns" | "rows">): string {
  const lines = [s.columns.map((c) => cell(c.label)).join(",")];
  for (const r of s.rows) {
    const indent = "  ".repeat(Math.max(0, (r._indent ?? 0) - 1));
    lines.push(s.columns.map((c, i) => cell(i === 0 ? `${indent}${r[c.name] ?? ""}` : r[c.name])).join(","));
  }
  return lines.join("\n");
}
