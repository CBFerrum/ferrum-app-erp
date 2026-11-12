// Promotions (src/promotion.rs, packages/selling/entities/selling.yaml): the test basket's request and
// answer, which promotion took what, and the Promotions Given report grouped by promotion. Pure; the
// pricing is the server's `price_lines`, over the promotions as saved, and nothing is saved by it.

import { add, cmp, sum } from "./decimal";

export interface BasketLine {
  item: string | null;
  qty: string;
  rate: string;
  discount: string;
}

export interface Priced {
  lines: { item: string; item_name?: string | null; qty: string; uom?: string | null; price_list_rate?: string | null; discount_percent?: string | null; promotion_percent?: string | null; added_by_promotion?: boolean | null; rate: string; amount: string; net_amount?: string | null }[];
  promotions: { promotion: string | null; title: string | null; line: number | null; amount: string; code: string | null }[];
  rejected_codes: (string | { code: string; reason?: string })[];
  currency?: string | null;
  total?: string | null;
  net_total?: string | null;
  total_taxes?: string | null;
  grand_total?: string | null;
  rounded_total?: string | null;
}

/** `at` as the server reads it: `YYYY-MM-DD HH:MM`; empty when none was given (the company's now). */
export const atOf = (local: string): string | undefined => (local ? local.replace("T", " ") : undefined);

/** The lines the server prices, from the rows a person filled: a row without an item or a quantity is left out. */
export const linesOf = (rows: BasketLine[]) =>
  rows
    .filter((r) => r.item && cmp(r.qty || "0", "0") > 0)
    .map((r) => ({ item: r.item, qty: r.qty, ...(r.rate.trim() ? { price_list_rate: r.rate.trim() } : {}), ...(r.discount.trim() ? { discount_percent: r.discount.trim() } : {}) }));

/** The company's `price_lines` call; its `lines` go as JSON text, as the parameter is declared. */
export function basketBody(o: { customer: string; rows: BasketLine[]; channel: string; place: string | null; at: string; codes: string }) {
  const body: Record<string, unknown> = { customer: o.customer, lines: JSON.stringify(linesOf(o.rows)) };
  if (o.channel.trim()) body.channel = o.channel.trim();
  if (o.place) body.place = o.place;
  if (atOf(o.at)) body.at = atOf(o.at);
  if (o.codes.trim()) body.codes = o.codes.trim();
  return body;
}

/** What one promotion took off this basket, or null when it did not apply. */
export function took(p: Priced, promotion: string | null): { amount: string; lines: number[] } | null {
  if (!promotion) return null;
  const mine = p.promotions.filter((x) => x.promotion === promotion);
  if (!mine.length) return null;
  return { amount: sum(mine.map((x) => x.amount)), lines: [...new Set(mine.map((x) => x.line).filter((x): x is number => x !== null))] };
}

/** A rejected code as text, whether the server sent it bare or with its reason. */
export const rejectedCode = (c: Priced["rejected_codes"][number]): { code: string; reason: string | null } => (typeof c === "string" ? { code: c, reason: null } : { code: c.code, reason: c.reason ?? null });

export interface Given {
  promotion: string;
  promotion_id: string | null;
  period: string;
  channel: string;
  documents: number;
  discount: string;
}

export interface GivenGroup {
  key: string;
  promotion: string;
  promotion_id: string | null;
  discount: string;
  rows: Given[];
}

/** The report's rows under each promotion, with what it gave in all; promotions in the order the report has them. */
export function groupGiven(rows: Given[]): GivenGroup[] {
  const by = new Map<string, GivenGroup>();
  for (const r of rows) {
    const key = r.promotion_id ?? `t:${r.promotion}`;
    const g = by.get(key) ?? { key, promotion: r.promotion, promotion_id: r.promotion_id, discount: "0", rows: [] };
    g.rows.push(r);
    g.discount = add(g.discount, r.discount);
    by.set(key, g);
  }
  return [...by.values()];
}

/** The Promotion fields, in the order a person thinks of them; the lists (customers, places, items) are the record's own. */
export const GROUPS: { id: string; fields: string[] }[] = [
  { id: "name", fields: ["title", "company", "disabled"] },
  { id: "schedule", fields: ["valid_from", "valid_upto", "weekdays", "from_time", "to_time"] },
  { id: "conditions", fields: ["channels", "requires_code", "first_order_only", "min_amount", "min_qty"] },
  { id: "reward", fields: ["gives", "percent", "amount", "buy_qty", "get_qty", "get_at", "cheapest_first", "free_item", "free_qty"] },
  { id: "limits", fields: ["max_uses", "used_count", "max_discount"] },
  { id: "stacking", fields: ["priority", "combinable", "with_line_discounts"] },
];
