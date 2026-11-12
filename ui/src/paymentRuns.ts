// Payment runs: the shapes the Finance add-on answers (addons/finance/src/payment_run.rs and
// packages/finance/entities/payment_run.yaml) and what a desk works out from them. Pure, so it is
// tested without a browser; the proposal, the discounts and the payments are the server's.

import { add, cmp, isDecimal, sub, sum } from "./decimal";

export interface RunLine {
  id?: string;
  pay: boolean;
  supplier?: string | null;
  supplier_name?: string | null;
  invoice_number?: string | null;
  invoice_type?: string | null;
  due_date?: string | null;
  outstanding: string;
  discount: string;
  amount: string;
  instrument_no?: string | null;
  payment_entry?: string | null;
  state?: "Proposed" | "Paid" | "Failed" | null;
  error?: string | null;
}

export interface Run {
  id: string;
  version: number;
  docstatus: 0 | 1 | 2;
  number?: string | null;
  company: string;
  posting_date: string;
  bank_account: string;
  due_by: string;
  take_discounts?: boolean;
  supplier?: string | null;
  supplier_group?: string | null;
  minimum_amount?: string | null;
  currency_code?: string | null;
  total_amount?: string | null;
  paid_amount?: string | null;
  failed_count?: number | null;
  run_status: "Proposed" | "Approved" | "Paid" | "Partly Paid";
  lines: RunLine[];
}

/** What the person typed per line, by the line's id. */
export type Edits = Record<string, Partial<Pick<RunLine, "pay" | "discount" | "amount" | "instrument_no">>>;

export const lineOf = (l: RunLine, e: Edits): RunLine => ({ ...l, ...(l.id ? e[l.id] : undefined) });

/** What the run would pay and the discounts it takes, over the lines as edited. */
export function totals(lines: RunLine[], e: Edits = {}) {
  const on = lines.map((l) => lineOf(l, e)).filter((l) => l.pay);
  return {
    count: on.length,
    suppliers: new Set(on.map((l) => l.supplier)).size,
    pay: sum(on.map((l) => l.amount)),
    discount: sum(on.map((l) => l.discount)),
    outstanding: sum(on.map((l) => l.outstanding)),
  };
}

/** A line the server would refuse: it pays nothing, or with its discount more than is outstanding (validate in payment_run.rs). */
export function problem(l: RunLine): "nothing" | "over" | null {
  if (!l.pay) return null;
  if (!isDecimal(l.amount) || cmp(l.amount, "0") <= 0) return "nothing";
  if (cmp(add(l.amount, l.discount || "0"), l.outstanding) > 0) return "over";
  return null;
}

/** Paying less than is outstanding, with the discount, leaves this much of the bill for a later run. */
export const left = (l: RunLine): string => sub(l.outstanding, add(l.amount || "0", l.discount || "0"));

/** The PATCH body that keeps the edits: the table goes whole, each line with its id and the fields a person may write. */
export function patchBody(lines: RunLine[], e: Edits) {
  return {
    lines: lines.map((l) => {
      const x = lineOf(l, e);
      return { id: x.id, pay: x.pay, discount: x.discount || "0", amount: x.amount || "0", instrument_no: x.instrument_no || null };
    }),
  };
}

export const edited = (e: Edits): boolean => Object.keys(e).length > 0;

/** Lines the last payments did not pay, in the order they stand. */
export const failed = (lines: RunLine[]): RunLine[] => lines.filter((l) => l.pay && l.state === "Failed");

/** The distinct payments the run made, for the remittance messages sent for each. */
export const payments = (lines: RunLine[]): string[] => [...new Set(lines.map((l) => l.payment_entry).filter((x): x is string => !!x))];

/** The body that makes the draft and what proposes into it; blank filters are left out. */
export function createBody(f: {
  company: string;
  posting_date: string;
  bank_account: string;
  due_by: string;
  take_discounts: boolean;
  mode_of_payment?: string | null;
  bank?: string | null;
  bank_file?: string;
  supplier?: string | null;
  supplier_group?: string | null;
  minimum_amount?: string;
}) {
  const out: Record<string, unknown> = { ...f };
  for (const k of Object.keys(out)) if (out[k] === "" || out[k] === null || out[k] === undefined) delete out[k];
  return out;
}
