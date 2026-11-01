// Revaluing foreign currency balances: the day it usually runs at, the document the server is
// asked about, and what its net result is called. Pure, so it is tested without a browser; the
// balances, rates and gains are the server's.

import { cmp } from "./decimal";

export interface Balance {
  id?: string;
  account: string;
  customer?: string | null;
  supplier?: string | null;
  currency: string;
  /** In its own currency; debit positive, credit negative. */
  balance: string;
  book_value: string;
  exchange_rate: string;
  revalued: string;
  gain_loss: string;
}

export interface Draft {
  rows: Balance[];
  total_gain_loss: string;
  gain_loss_account?: string | null;
  reversal_date?: string | null;
}

const pad = (n: number) => String(n).padStart(2, "0");

/** The day a period-end revaluation usually runs at: today when it is a month's last day,
 *  else the last day of the month before. */
export function defaultDay(today: string): string {
  const [y, m, d] = today.split("-").map(Number);
  const last = new Date(Date.UTC(y, m, 0)).getUTCDate();
  if (d === last) return today;
  const [py, pm] = m === 1 ? [y - 1, 12] : [y, m - 1];
  return `${py}-${pad(pm)}-${pad(new Date(Date.UTC(py, pm, 0)).getUTCDate())}`;
}

/** The Exchange Revaluation the server is asked to work out and, once confirmed, to post. */
export function bodyOf(o: { company: string; date: string; reverse: boolean; reversalDate: string; account: string | null }) {
  return {
    company: o.company,
    posting_date: o.date,
    auto_reverse: o.reverse,
    reversal_date: o.reverse && o.reversalDate ? o.reversalDate : undefined,
    gain_loss_account: o.account || undefined,
  };
}

/** Whether the revaluation, as a whole, gains or loses. */
export function outcome(total: string): "gain" | "loss" | "none" {
  const c = cmp(total, "0");
  return c > 0 ? "gain" : c < 0 ? "loss" : "none";
}

/** The party of a balance: its customer or supplier, if it has one. */
export function party(b: Pick<Balance, "customer" | "supplier">): { to: "Customer" | "Supplier"; id: string } | null {
  return b.customer ? { to: "Customer", id: b.customer } : b.supplier ? { to: "Supplier", id: b.supplier } : null;
}
