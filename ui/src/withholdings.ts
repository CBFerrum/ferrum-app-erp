// Withholding (src/withholding.rs, the `withholding` report in src/reports.rs): categories with their
// dated rates and thresholds as the server has them, and what a period withheld or collected grouped
// by category and by party. Pure. No category is seeded by the engine: they are records a country's
// add-on or an accountant makes, so nothing here names one.

import { add } from "./decimal";

export interface Rate {
  id?: string;
  valid_from: string;
  rate: string;
  single_threshold?: string | null;
  yearly_threshold?: string | null;
}

export interface Category {
  id: string;
  category_name: string;
  applies: "On Bill" | "On Payment" | "On Sale";
  base: "Net Total" | "Grand Total";
  cumulative_base: "Whole Year" | "Above Threshold";
  round_to_unit?: boolean;
  description?: string | null;
  rates: Rate[];
  accounts: { company: string; account: string }[];
}

export interface Line {
  category: string;
  party: string | null;
  posting_date: string;
  voucher_type: string;
  voucher_no: string | null;
  voucher_id: string;
  against: string | null;
  certificate: string | null;
  base: string;
  taxable: string;
  rate: string;
  amount: string;
}

export interface Report {
  rows: Line[];
  totals?: { amount?: string } | null;
  meta?: { from_date?: string; to_date?: string; truncated?: boolean } | null;
}

/** The rate in force on a day: the latest whose date has come. */
export const inForce = (rates: Rate[], day: string): Rate | null => rates.filter((r) => r.valid_from <= day).sort((a, b) => b.valid_from.localeCompare(a.valid_from))[0] ?? null;

export interface Sum {
  key: string;
  base: string;
  taxable: string;
  amount: string;
  lines: Line[];
}

function fold(rows: Line[], key: (l: Line) => string): Sum[] {
  const by = new Map<string, Sum>();
  for (const l of rows) {
    const k = key(l);
    const s = by.get(k) ?? { key: k, base: "0", taxable: "0", amount: "0", lines: [] };
    s.base = add(s.base, l.base);
    s.taxable = add(s.taxable, l.taxable);
    s.amount = add(s.amount, l.amount);
    s.lines.push(l);
    by.set(k, s);
  }
  return [...by.values()];
}

/** What each category withheld or collected, in the order the report has them. */
export const byCategory = (rows: Line[]): Sum[] => fold(rows, (l) => l.category);

/** Each party's lines (a party the report does not name stays together under an empty key). */
export const byParty = (rows: Line[]): Sum[] => fold(rows, (l) => l.party ?? "");

/** Rows for one party, matched as typed (a part of the name, any case). */
export const forParty = (rows: Line[], typed: string): Line[] => {
  const q = typed.trim().toLowerCase();
  return q ? rows.filter((l) => (l.party ?? "").toLowerCase().includes(q)) : rows;
};

const cell = (v: string | null | undefined) => {
  const x = v ?? "";
  // A text that starts like a formula is not one in a spreadsheet.
  const safe = /^[=+\-@]/.test(x) && !/^-?\d+(\.\d+)?$/.test(x) ? `'${x}` : x;
  return /[",\n]/.test(safe) ? `"${safe.replace(/"/g, '""')}"` : safe;
};

/** A party's statement for the period as a file: one line per document, with the heads the person sees. */
export function statementCsv(rows: Line[], heads: string[]): string {
  const body = rows.map((l) => [l.category, l.party, l.posting_date, l.voucher_no, l.against, l.certificate, l.base, l.taxable, l.rate, l.amount].map(cell).join(","));
  return [heads.map(cell).join(","), ...body].join("\r\n") + "\r\n";
}
