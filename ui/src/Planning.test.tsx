// The planning desk for the answers of an MRP run, an order and `availability` (src/manufacturing.rs,
// src/reservation.rs), rendered to HTML, and its arrangement of the plan.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Planning } from "./Planning";
import { acceptBody, availabilityBody, buckets, defaults, held, keeps, needSupplier, open, reservable, tally, type MrpLine, type MrpRun, type Order } from "./plans";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "all", cancel: "none", export: "all", import: "none" };
const line = (over: Partial<MrpLine> = {}): MrpLine => ({ id: "l1", item: "i1", item_name: "Bolt", action: "Purchase", bucket_start: "2026-05-04", gross_requirement: "100", available: "20", on_order: "0", net_requirement: "80", suggested_qty: "80", warehouse: "w1", required_by: "2026-05-04", ...over });
const run = (over: Partial<MrpRun> = {}): MrpRun => ({ id: "m1", version: 3, docstatus: 0, number: "MRP-1", bucket: "Week", status: "Planned", messages: [], lines: [line(), line({ id: "l2", action: "Manufacture", bucket_start: "2026-05-11", suggested_qty: "40", net_requirement: "40" }), line({ id: "l3", item: "i2", item_name: "Nut", suggested_qty: "0" })], ...over });
const order = (over: Partial<Order> = {}): Order => ({ id: "o1", version: 1, company: "co", docstatus: 1, status: "To Deliver", per_delivered: 0, items: [{ id: "x1", item: "i1", qty: "10", warehouse: "w1", delivery_date: "2026-05-10", available_qty: "4", promised_date: "2026-05-14", stock_reserved_qty: "4", backorder_qty: "6", reserved_until: "2026-05-20" }], ...over });

function screen(search: string, seed: (qc: QueryClient) => void) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: ["MrpRun", "SalesOrder"].map((name) => ({ name, label: name, label_plural: name, kind: "document", submittable: true, permissions: perms })), modules: [], packages: [] });
  seed(qc);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Planning />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the planning desk", () => {
  it("asks for a run first", () => {
    expect(text(screen("", () => undefined))).toContain("Choose a run");
  });

  it("shows the planned orders per item and period with what is needed, and what is already made", () => {
    const t = text(screen("?run=m1", (qc) => qc.setQueryData(["erp", "mrp", "m1"], run({ lines: [line(), line({ id: "l2", action: "Manufacture", bucket_start: "2026-05-11", suggested_qty: "40", created_document: "WorkOrder WO-1" })] }))));
    expect(t).toContain("Buy 80.00");
    expect(t).toContain("needs 80.00, has 20.00, on order 0.00");
    expect(t).toContain("Make 40.00 WorkOrder WO-1");
    expect(t).toContain("Make the orders taken");
  });

  it("lists the orders already placed that should change", () => {
    const t = text(screen("?run=m1", (qc) => qc.setQueryData(["erp", "mrp", "m1"], run({ messages: [{ message: "Push Out", document_type: "PurchaseOrder", document: "PO-7", current_date: "2026-05-01", suggested_date: "2026-05-20" }] }))));
    expect(t).toContain("Push out");
    expect(t).toContain("PO-7");
  });

  it("says plainly when nothing is planned, and shows the server's refusal when a run cannot be opened", () => {
    expect(text(screen("?run=m1", (qc) => qc.setQueryData(["erp", "mrp", "m1"], run({ lines: [], status: "Draft" }))))).toContain("Nothing is planned");
    const failed = text(screen("?run=m1", (qc) => qc.getQueryCache().build(qc, { queryKey: ["erp", "mrp", "m1"] }).setState({ status: "error", error: new Error("no such run"), fetchStatus: "idle" })));
    expect(failed).toContain("no such run");
  });

  it("shows the promised date of each line and says when it is later than wanted", () => {
    const t = text(screen("?tab=promise&order=o1", (qc) => qc.setQueryData(["erp", "plan-order", "o1"], order())));
    expect(t).toContain("Later than wanted");
    expect(t).toContain("Check now");
  });

  it("shows what is held for each line under a holding policy", () => {
    const t = text(
      screen("?tab=reserve&order=o1", (qc) => {
        qc.setQueryData(["erp", "plan-order", "o1"], order());
        qc.setQueryData(["erp", "plan-company", "co"], { reservation_policy: "Hold" });
      }),
    );
    expect(t).toContain("This company holds stock for orders");
    expect(t).toContain("10.00 4.00 6.00");
    expect(t).toContain("Reserve stock");
  });

  it("says that nothing is held where the policy only counts", () => {
    const t = text(
      screen("?tab=reserve&order=o1", (qc) => {
        qc.setQueryData(["erp", "plan-order", "o1"], order());
        qc.setQueryData(["erp", "plan-company", "co"], { reservation_policy: "Count Only" });
      }),
    );
    expect(t).toContain("no stock is held");
  });
});

describe("the plan's arrangement", () => {
  it("arranges lines per item and warehouse under the periods in order", () => {
    const { periods, rows } = buckets(run().lines);
    expect(periods).toEqual(["2026-05-04", "2026-05-11"]);
    expect(rows.map((r) => [r.item, [...r.cells.keys()]])).toEqual([["i1", ["2026-05-04", "2026-05-11"]], ["i2", ["2026-05-04"]]]);
  });

  it("takes every open line by default, and asks for a supplier on a purchase taken without one", () => {
    const lines = run().lines;
    expect([...defaults(lines)]).toEqual(["l1", "l2"]);
    expect(open(lines[2])).toBe(false);
    expect(needSupplier(lines, defaults(lines), {}).map((l) => l.id)).toEqual(["l1"]);
    expect(needSupplier(lines, defaults(lines), { l1: "s1" })).toEqual([]);
    expect(tally(lines, defaults(lines))).toEqual({ Purchase: 1, Manufacture: 1, Transfer: 0 });
  });

  it("sets what is left out to nothing and keeps what is taken and what was made", () => {
    const lines = [line(), line({ id: "l2", suggested_qty: "40" }), line({ id: "l3", suggested_qty: "9", created_document: "PurchaseOrder PO-1" })];
    const body = acceptBody(lines, new Set(["l1"]), { l1: "s1" });
    expect(body.lines.map((l) => [l.id, l.suggested_qty, l.supplier])).toEqual([["l1", "80", "s1"], ["l2", "0", null], ["l3", "9", null]]);
  });

  it("asks availability for the line's item, quantity and warehouse, and reads the promise and the holds", () => {
    expect(availabilityBody({ item: "i1", qty: " 10 ", warehouse: "w1", date: null })).toEqual({ item: "i1", qty: "10", warehouse: "w1" });
    expect(keeps({ promised_date: "2026-05-14", delivery_date: "2026-05-10" })).toBe(false);
    expect(keeps({ promised_date: null, delivery_date: "2026-05-10" })).toBeNull();
    expect(held({ item: "i", qty: "1" })).toEqual({ reserved: "0", short: "0" });
    expect([reservable(order()), reservable(order({ status: "Closed" })), reservable(order({ docstatus: 0 })), reservable(order({ per_delivered: 100 }))]).toEqual([true, false, false, false]);
  });
});
