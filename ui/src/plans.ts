// Planning (src/planning.rs, src/manufacturing.rs, src/reservation.rs): the MRP run's lines as buckets
// per item and warehouse, which suggested orders are accepted, what is sent to make them, and the
// shapes of the availability answer and of an order line's reservation. Pure. The numbers, the periods
// and the suggestions are the server's; this arranges them.

import { cmp, isDecimal } from "./decimal";

export interface MrpLine {
  id?: string;
  item: string;
  item_name?: string | null;
  action: "Purchase" | "Manufacture" | "Transfer";
  source_warehouse?: string | null;
  bucket_start?: string | null;
  gross_requirement?: string | null;
  available?: string | null;
  on_order?: string | null;
  safety_stock?: string | null;
  net_requirement?: string | null;
  suggested_qty: string;
  supplier?: string | null;
  warehouse?: string | null;
  required_by?: string | null;
  created_document?: string | null;
}

export interface MrpMessage {
  message: "Bring Forward" | "Push Out" | "Cancel" | "Increase" | "Decrease";
  document_type?: string | null;
  document?: string | null;
  item?: string | null;
  current_date?: string | null;
  current_qty?: string | null;
  suggested_date?: string | null;
  suggested_qty?: string | null;
}

export interface MrpRun {
  id: string;
  version: number;
  docstatus: 0 | 1 | 2;
  number?: string | null;
  bucket?: "Day" | "Week" | "Month" | null;
  status: "Draft" | "Planned" | "Orders Created";
  lines: MrpLine[];
  messages: MrpMessage[];
}

export interface Row {
  key: string;
  item: string;
  item_name: string;
  warehouse: string | null;
  cells: Map<string, MrpLine[]>;
}

/** The periods the run has, in order, and its lines arranged per item and warehouse (the first bucket's order is the run's own). */
export function buckets(lines: MrpLine[]): { periods: string[]; rows: Row[] } {
  const periods = [...new Set(lines.map((l) => l.bucket_start ?? ""))].sort();
  const by = new Map<string, Row>();
  for (const l of lines) {
    const key = `${l.item}|${l.warehouse ?? ""}`;
    const row = by.get(key) ?? { key, item: l.item, item_name: l.item_name ?? "", warehouse: l.warehouse ?? null, cells: new Map() };
    const p = l.bucket_start ?? "";
    row.cells.set(p, [...(row.cells.get(p) ?? []), l]);
    by.set(key, row);
  }
  return { periods, rows: [...by.values()] };
}

/** A line that can still be turned into an order: it suggests something and has not been made yet. */
export const open = (l: MrpLine): boolean => !l.created_document && isDecimal(l.suggested_qty) && cmp(l.suggested_qty, "0") > 0;

/** Which lines are taken when nothing was changed: every open one. */
export const defaults = (lines: MrpLine[]): Set<string> => new Set(lines.filter((l) => l.id && open(l)).map((l) => l.id as string));

export type Supplied = Record<string, string | null>;

/** Purchase lines taken without a supplier: the server refuses them, so the desk says which before. */
export const needSupplier = (lines: MrpLine[], taken: Set<string>, sup: Supplied): MrpLine[] =>
  lines.filter((l) => l.id && taken.has(l.id) && l.action === "Purchase" && !(l.id in sup ? sup[l.id] : l.supplier));

/**
 * The lines as saved before orders are made: what is taken keeps its suggestion (and a supplier given
 * here); what is left out is set to nothing, which is how the server leaves a line unordered. A line
 * already made keeps what it has.
 */
export function acceptBody(lines: MrpLine[], taken: Set<string>, sup: Supplied) {
  return {
    lines: lines.map((l) => ({
      id: l.id,
      item: l.item,
      action: l.action,
      source_warehouse: l.source_warehouse ?? null,
      supplier: l.id && l.id in sup ? sup[l.id] : (l.supplier ?? null),
      warehouse: l.warehouse ?? null,
      required_by: l.required_by ?? null,
      suggested_qty: l.created_document || (l.id && taken.has(l.id)) ? l.suggested_qty : "0",
    })),
  };
}

/** What taking these lines would make, by action. */
export function tally(lines: MrpLine[], taken: Set<string>): Record<MrpLine["action"], number> {
  const out = { Purchase: 0, Manufacture: 0, Transfer: 0 };
  for (const l of lines) if (l.id && taken.has(l.id) && open(l)) out[l.action] += 1;
  return out;
}

export interface Availability {
  item: string;
  qty: string;
  lead_time_days?: number | null;
  promise_date: string | null;
  warehouses: { warehouse: string; on_hand: string; reserved: string; held: string; available_now: string; incoming: { date: string; qty: string; from: string; id: string }[]; promise_date: string | null }[];
}

/** The `availability` call for an order line. */
export const availabilityBody = (o: { item: string; qty: string; warehouse?: string | null; date?: string | null }) => ({
  item: o.item,
  ...(o.qty.trim() ? { qty: o.qty.trim() } : {}),
  ...(o.warehouse ? { warehouse: o.warehouse } : {}),
  ...(o.date ? { date: o.date } : {}),
});

export interface OrderLine {
  id?: string;
  item: string;
  item_name?: string | null;
  qty: string;
  warehouse?: string | null;
  delivery_date?: string | null;
  available_qty?: string | null;
  promised_date?: string | null;
  stock_reserved_qty?: string | null;
  backorder_qty?: string | null;
  reserved_until?: string | null;
}

export interface Order {
  id: string;
  version: number;
  number?: string | null;
  company: string;
  docstatus: 0 | 1 | 2;
  status?: string | null;
  per_delivered?: string | number | null;
  items: OrderLine[];
}

/** Whether the promise can be kept: promised by the day the line is wanted. */
export function keeps(l: Pick<OrderLine, "promised_date" | "delivery_date">): boolean | null {
  return l.promised_date && l.delivery_date ? l.promised_date <= l.delivery_date : null;
}

/** The part of a line that is held, short, and how it stands: nothing is held under a policy that only counts. */
export const held = (l: OrderLine): { reserved: string; short: string } => ({ reserved: l.stock_reserved_qty ?? "0", short: l.backorder_qty ?? "0" });

/** Whether a reservation is offered on an order in this state (the conditions of Reserve Stock and Release Reservation). */
export const reservable = (o: Pick<Order, "docstatus" | "status" | "per_delivered">): boolean => o.docstatus === 1 && o.status !== "Closed" && Number(o.per_delivered ?? 0) < 100;
