// The finance overview's rules: which of its rows ask for attention, in what order, and the words
// for them. Pure, so they are tested without a browser. The counts and amounts are the server's.

import { t } from "@ferrum/client";

export interface Overview {
  rows: OverviewRow[];
  meta: { as_of: string };
}

export interface OverviewRow {
  key: string;
  item: string;
  count?: number;
  amount?: string;
  oldest?: string | null;
  detail?: Detail[];
}

export interface Detail {
  id?: string;
  name?: string;
  count?: number;
  oldest?: string | null;
  balance?: string;
  entity?: string;
  label?: string;
  fiscal_year?: string;
  start?: string;
  end?: string;
}

/** The rows that are counts of things to do, most pressing first. */
const URGENCY = ["overdue_receivables", "overdue_payables", "unreconciled_lines", "drafts", "open_months", "unclosed_years"];

export const row = (o: Overview, key: string): OverviewRow | undefined => o.rows.find((r) => r.key === key);

/** The rows with something to do, most pressing first. */
export function needing(o: Overview): OverviewRow[] {
  return URGENCY.flatMap((key) => {
    const r = row(o, key);
    return r && (r.count ?? 0) > 0 ? [r] : [];
  });
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** One row in a line of words. */
export function say(r: OverviewRow): string {
  const n = r.count ?? 0;
  switch (r.key) {
    case "overdue_receivables":
      return n === 1 ? t("1 overdue invoice to collect") : t("{n} overdue invoices to collect", { n });
    case "overdue_payables":
      return n === 1 ? t("1 overdue bill to pay") : t("{n} overdue bills to pay", { n });
    case "unreconciled_lines":
      return n === 1 ? t("1 bank line to reconcile") : t("{n} bank lines to reconcile", { n });
    case "drafts":
      return n === 1 ? t("1 draft waiting") : t("{n} drafts waiting", { n });
    case "open_months":
      return n === 1 ? t("1 month not closed") : t("{n} months not closed", { n });
    case "unclosed_years":
      return n === 1 ? t("1 fiscal year not closed") : t("{n} fiscal years not closed", { n });
    default:
      return r.item;
  }
}

/** "Oct 2026" for a month's first day (`YYYY-MM-DD`). */
export function monthName(start: string): string {
  const [y, m] = start.split("-").map(Number);
  return `${["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m - 1]} ${y}`;
}

/** Where a row leads. */
export function destination(r: OverviewRow, company: string | null, asOf: string): string | null {
  const q = (more: Record<string, string>) => `?${new URLSearchParams({ ...(company ? { company } : {}), ...more, run: "1" })}`;
  switch (r.key) {
    case "overdue_receivables":
    case "receivables_due_week":
      return `/r/receivables${q({ as_of: asOf })}`;
    case "overdue_payables":
    case "payables_due_week":
      return `/r/payables${q({ as_of: asOf })}`;
    case "unreconciled_lines":
      return "/r/bank_reconciliation";
    case "unclosed_years":
      return "/e/FiscalYear";
    default:
      return null;
  }
}

/** The list a draft kind opens. */
export const draftsPath = (entity: string): string => `/e/${entity}`;
