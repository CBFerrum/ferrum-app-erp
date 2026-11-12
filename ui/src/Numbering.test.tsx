// The numbering desk for the answers of the series list (src/series.rs, masters.yaml NumberSeries),
// rendered to HTML, and its default-switching plan.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Numbering } from "./Numbering";
import { byKind, defaultOf, makeDefaultPlan, numberedBy, took, type Series } from "./series";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
const s = (over: Partial<Series>): Series => ({ id: "s1", version: 2, series_name: "Tax invoices", document: "SalesInvoice", template: "TINV-{FY}-{seq:5}", gapless: true, use_by_default: true, ...over });
const rows = [s({}), s({ id: "s2", series_name: "Receipts", template: "RCPT-{seq:4}", gapless: false, use_by_default: false }), s({ id: "s3", series_name: "Orders", document: "SalesOrder", use_by_default: false, disabled: true })];

function screen(search: string, seed: (qc: QueryClient) => void) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: ["NumberSeries", "SalesInvoice", "SalesOrder"].map((name) => ({ name, label: name === "SalesInvoice" ? "Sales Invoice" : name === "SalesOrder" ? "Sales Order" : "Numbering Series", label_plural: name, kind: "master", submittable: false, permissions: perms })), modules: [], packages: [] });
  qc.setQueryData(["meta", "NumberSeries"], { name: "NumberSeries", label: "Numbering Series", fields: [{ name: "document", label: "For", type: "select", options: ["SalesOrder", "SalesInvoice"], can_read: true, can_write: true }], form: [], children: {}, permissions: perms });
  seed(qc);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Numbering />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the numbering desk", () => {
  it("lists the series under their kinds with the template, the default flag and the last number given", () => {
    const t = text(
      screen("", (qc) => {
        qc.setQueryData(["erp", "series"], { rows });
        qc.setQueryData(["erp", "series-last", "s1"], { rows: [{ number: "TINV-2026-00041" }] });
        qc.setQueryData(["erp", "series-last", "s2"], { rows: [] });
      }),
    );
    expect(t).toContain("Sales Order");
    expect(t).toContain("Sales Invoice");
    expect(t).toContain("TINV-{FY}-{seq:5}");
    expect(t).toContain("TINV-2026-00041");
    expect(t).toContain("Default Gapless");
    expect(t).toContain("Switched off");
    expect(t).toContain("None yet");
    expect(t).toContain("not shown");
  });

  it("offers to make a series the default only where it is not one and not switched off", () => {
    const html = screen("", (qc) => qc.setQueryData(["erp", "series"], { rows }));
    expect((html.match(/Use by default/g) ?? []).length).toBe(1);
  });

  it("says plainly when there is no series, and shows the server's refusal when the list cannot load", () => {
    expect(text(screen("", (qc) => qc.setQueryData(["erp", "series"], { rows: [] })))).toContain("No series yet");
    expect(text(screen("", (qc) => qc.getQueryCache().build(qc, { queryKey: ["erp", "series"] }).setState({ status: "error", error: new Error("no access to series"), fetchStatus: "idle" })))).toContain("no access to series");
  });

  it("says which series a document took, or that it named none", () => {
    const named = text(screen("?tab=document&kind=SalesInvoice&doc=d1", (qc) => qc.setQueryData(["erp", "series-doc", "SalesInvoice", "d1"], { number: "TINV-2026-00001", number_series: "s1" })));
    expect(named).toContain("It is numbered TINV-2026-00001.");
    expect(named).toContain("It took the series");
    const none = text(screen("?tab=document&kind=SalesInvoice&doc=d1", (qc) => qc.setQueryData(["erp", "series-doc", "SalesInvoice", "d1"], { number: "SINV-1" })));
    expect(none).toContain("It names no series");
  });
});

describe("the series' arrangement", () => {
  it("groups by kind in the metadata's order and leaves out kinds with none", () => {
    expect(byKind(rows, ["SalesOrder", "SalesInvoice", "DeliveryNote"]).map((g) => [g.kind, g.series.map((x) => x.series_name)])).toEqual([["SalesOrder", ["Orders"]], ["SalesInvoice", ["Receipts", "Tax invoices"]]]);
  });

  it("makes a series the default by giving up the one that was first, for the same company", () => {
    expect(makeDefaultPlan(rows, rows[1])).toEqual([{ id: "s1", version: 2, use_by_default: false }, { id: "s2", version: 2, use_by_default: true }]);
    expect(makeDefaultPlan(rows, rows[2])).toEqual([{ id: "s3", version: 2, use_by_default: true }]);
    expect(defaultOf(rows, { document: "SalesInvoice", company: "c1" })).toBeUndefined();
    expect(defaultOf(rows, rows[1])?.id).toBe("s1");
  });

  it("reads what a document took and finds the documents a series numbered", () => {
    expect(took({ number: "N", number_series: "s1" })).toEqual({ series: "s1", number: "N" });
    expect(took({})).toEqual({ series: null, number: null });
    expect(numberedBy("s1")).toBe('{"number_series":"s1"}');
  });
});
