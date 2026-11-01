// Applying a party's unapplied money to their open invoices: which amount goes where, oldest first,
// and what is left over on each side. Pure, so the screen's rules are tested without a browser.
// Amounts are the server's decimal strings, added and compared exactly (decimal.ts); the server
// still decides what holds (it refuses more than is left, another party, another currency).

import { abs, add, cmp, isDecimal, isZero, min, normalize, sub, sum } from "./decimal";

/** An open item of a party, as the receivables and payables reports list it. */
export interface Open {
  /** The open item's id: what an allocation names. */
  id: string;
  kind: string;
  number: string | null;
  date: string;
  due: string | null;
  currency: string;
  /** Positive: owed on an invoice. Negative: money to apply (a payment, a credit note, a journal credit). */
  outstanding: string;
}

interface ReportRow {
  voucher_id: string;
  voucher_type: string;
  voucher_no?: string | null;
  posting_date: string;
  due_date?: string | null;
  currency: string;
  outstanding: string;
}

/** The party's open items from a receivables or payables report (what is settled is left out). */
export function fromReport(rows: ReportRow[]): Open[] {
  return rows
    .filter((r) => !isZero(r.outstanding))
    .map((r) => ({ id: r.voucher_id, kind: r.voucher_type, number: r.voucher_no ?? null, date: r.posting_date, due: r.due_date ?? null, currency: r.currency, outstanding: r.outstanding }));
}

/** Words for what an item is. */
export function kindLabel(item: Pick<Open, "kind" | "outstanding">): string {
  switch (item.kind) {
    case "PaymentEntry":
      return "Payment";
    case "JournalEntry":
      return "Journal credit";
    case "SalesInvoice":
    case "PurchaseInvoice":
      return cmp(item.outstanding, "0") < 0 ? "Credit note" : "Invoice";
    default:
      return item.kind;
  }
}

const byDate = (a: string, b: string) => a.localeCompare(b);

/** Money to apply, oldest first; invoices, the earliest due first. */
export function split(items: Open[], currency: string): { credits: Open[]; invoices: Open[] } {
  const mine = items.filter((i) => i.currency === currency);
  return {
    credits: mine.filter((i) => cmp(i.outstanding, "0") < 0).sort((a, b) => byDate(a.date, b.date) || byDate(a.number ?? "", b.number ?? "")),
    invoices: mine.filter((i) => cmp(i.outstanding, "0") > 0).sort((a, b) => byDate(a.due ?? a.date, b.due ?? b.date) || byDate(a.date, b.date) || byDate(a.number ?? "", b.number ?? "")),
  };
}

/** The currencies that have both money and invoices to join, then the rest. */
export function currencies(items: Open[]): string[] {
  const all = [...new Set(items.map((i) => i.currency))];
  const joinable = (c: string) => {
    const { credits, invoices } = split(items, c);
    return credits.length > 0 && invoices.length > 0;
  };
  return [...all.filter(joinable), ...all.filter((c) => !joinable(c))];
}

/** What each invoice would take if all the money went to the oldest invoices first. */
export function fill(credits: Open[], invoices: Open[]): Record<string, string> {
  let left = sum(credits.map((c) => abs(c.outstanding)));
  const out: Record<string, string> = {};
  for (const inv of invoices) {
    if (isZero(left)) break;
    const take = min(inv.outstanding, left);
    out[inv.id] = take;
    left = sub(left, take);
  }
  return out;
}

export interface Pairing {
  credit: string;
  invoice: string;
  amount: string;
}

/** Pairs the money, oldest first, with the invoices in order, each for the amount typed against
 *  it; `short` is what no money was left to cover. */
export function pairOff(credits: Open[], invoices: Open[], amounts: Record<string, string>): { pairings: Pairing[]; short: string } {
  const room = new Map(credits.map((c) => [c.id, abs(c.outstanding)]));
  const pairings: Pairing[] = [];
  let short = "0";
  for (const inv of invoices) {
    let need = isDecimal(amounts[inv.id]) && cmp(amounts[inv.id], "0") > 0 ? normalize(amounts[inv.id]) : "0";
    for (const c of credits) {
      if (isZero(need)) break;
      const have = room.get(c.id) ?? "0";
      if (isZero(have)) continue;
      const take = min(need, have);
      pairings.push({ credit: c.id, invoice: inv.id, amount: take });
      room.set(c.id, sub(have, take));
      need = sub(need, take);
    }
    short = add(short, need);
  }
  return { pairings, short };
}

export interface Tally {
  /** Money there is to apply. */
  available: string;
  /** Money the invoices would take. */
  applied: string;
  /** Money left over. */
  left: string;
  /** What each piece of money would have left, and each invoice. */
  creditLeft: Record<string, string>;
  invoiceLeft: Record<string, string>;
  /** Why it cannot be applied, per invoice, and as a whole. */
  errors: Record<string, string>;
  over: string | null;
}

export function tally(credits: Open[], invoices: Open[], amounts: Record<string, string>): Tally {
  const { pairings, short } = pairOff(credits, invoices, amounts);
  const creditLeft: Record<string, string> = {};
  for (const c of credits) creditLeft[c.id] = sub(abs(c.outstanding), sum(pairings.filter((p) => p.credit === c.id).map((p) => p.amount)));
  const invoiceLeft: Record<string, string> = {};
  const errors: Record<string, string> = {};
  for (const inv of invoices) {
    const typed = amounts[inv.id];
    const given = isDecimal(typed) ? typed : "0";
    invoiceLeft[inv.id] = sub(inv.outstanding, given);
    if (typed !== undefined && typed !== "" && (!isDecimal(typed) || cmp(typed, "0") < 0)) errors[inv.id] = "Enter an amount, or leave it empty";
    else if (cmp(given, inv.outstanding) > 0) errors[inv.id] = `Only ${normalize(inv.outstanding)} is owed on this`;
  }
  const available = sum(credits.map((c) => abs(c.outstanding)));
  const applied = sum(pairings.map((p) => p.amount));
  return { available, applied, left: sub(available, applied), creditLeft, invoiceLeft, errors, over: isZero(short) ? null : `That is ${normalize(short)} more than the money there is to apply` };
}

/** Whether there is something to confirm and nothing in the way. */
export const ready = (t: Tally): boolean => cmp(t.applied, "0") > 0 && !t.over && Object.keys(t.errors).length === 0;

/** The day an allocation can be dated: today, or later when a document of the pairings is. */
export function postingDate(today: string, dates: string[]): string {
  return [today, ...dates].reduce((latest, d) => (d > latest ? d : latest));
}

/** The Payment Allocation document for these pairings. */
export function documentBody(o: { company: string; partyType: "Customer" | "Supplier"; party: string; date: string; pairings: Pairing[]; remarks?: string }) {
  return {
    company: o.company,
    party_type: o.partyType,
    [o.partyType === "Customer" ? "customer" : "supplier"]: o.party,
    posting_date: o.date,
    remarks: o.remarks || undefined,
    allocations: o.pairings.map((p) => ({ credit: p.credit, invoice: p.invoice, allocated_amount: p.amount })),
  };
}

/** How many days past due an invoice is on `today` (negative: not due yet). */
export function daysPast(due: string | null, today: string): number | null {
  if (!due) return null;
  const [a, b] = [Date.parse(`${due}T00:00:00Z`), Date.parse(`${today}T00:00:00Z`)];
  return Number.isNaN(a) || Number.isNaN(b) ? null : Math.round((b - a) / 86_400_000);
}

/** "3 days overdue", "due in 2 days", "due today". */
export function dueWords(due: string | null, today: string): string {
  const days = daysPast(due, today);
  if (days === null) return "";
  if (days === 0) return "Due today";
  return days > 0 ? `${days} ${days === 1 ? "day" : "days"} overdue` : `Due in ${-days} ${days === -1 ? "day" : "days"}`;
}
