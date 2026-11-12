// The tax return workbench for the answers of the accounting package (shapes as in
// tests/tax_return_it.rs), rendered to HTML, and its pure parts.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";
import { TaxReturnDesk, TaxReturnStart } from "./TaxReturn";
import { adjustBody, boxOf, drillable, filedValue, needReason, startBody, status, type Definition, type TaxReturn } from "./taxReturns";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "all", cancel: "all", export: "all", import: "none" };
const box = (code: string, computed: string, over: object = {}) => ({ id: `b-${code}`, code, label: `Box ${code}`, computed, adjustment: "0", adjustment_reason: null, value: computed, ...over });
const ret = (over: Partial<TaxReturn> = {}): TaxReturn => ({ id: "r1", version: 2, docstatus: 0, number: "TR-2026-0001", definition: "d1", company: "co", from_date: "2026-05-01", to_date: "2026-05-31", boxes: [box("OUT", "15"), box("MAN", "0"), box("NET", "10")], ...over });
const def: Definition = { id: "d1", definition_name: "Monthly", frequency: "Monthly", boxes: [{ code: "OUT", kind: "Sum" }, { code: "MAN", kind: "By Hand" }, { code: "NET", kind: "Formula" }] };

function screen(path: string, seed: (qc: QueryClient) => void = () => undefined) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: [{ name: "TaxReturn", label: "Tax Return", label_plural: "Tax Returns", kind: "document", submittable: true, permissions: perms }], modules: [], packages: [] });
  seed(qc);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/desk/tax-return" element={<TaxReturnStart />} />
          <Route path="/desk/tax-return/:id" element={<TaxReturnDesk />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");
const seeded = (r: TaxReturn) => (qc: QueryClient) => {
  qc.setQueryData(["erp", "return", "r1"], r);
  qc.setQueryData(["erp", "return-def", "d1"], def);
  qc.setQueryData(["erp", "return-files", "r1", r.docstatus], { templates: [{ key: "accounting.tax_return_csv", label: "Tax return (CSV)", output: "csv" }, { key: "x.page", label: "Page", output: "page" }] });
};

describe("the tax return workbench", () => {
  it("asks for the return and the period before working one out", () => {
    const t = text(screen("/desk/tax-return"));
    expect(t).toContain("Period starts");
    expect(t).toContain("Work out the return");
  });

  it("shows each box with what the books give and how it is worked out, and files only from a draft", () => {
    const t = text(screen("/desk/tax-return/r1", seeded(ret())));
    expect(t).toContain("OUT Box OUT From the tax ledger 15.00");
    expect(t).toContain("Entered by hand");
    expect(t).toContain("Worked out from other boxes");
    expect(t).toContain("File and lock the period");
    expect(t).toContain("Tax return (CSV)");
    expect(t).not.toContain("Page");
  });

  it("says the period is closed once filed, and offers to cancel instead of filing", () => {
    const t = text(screen("/desk/tax-return/r1", seeded(ret({ docstatus: 1, filed_on: "2026-06-02" }))));
    expect(t).toContain("This period is closed to invoices");
    expect(t).toContain("Cancel the return");
    expect(t).not.toContain("File and lock the period");
  });

  it("shows the documents behind a box with their total", () => {
    const t = text(
      screen("/desk/tax-return/r1?box=OUT", (qc) => {
        seeded(ret())(qc);
        qc.setQueryData(["erp", "return-drill", "r1", "OUT", 2], {
          rows: [{ voucher_type: "SalesInvoice", voucher_id: "si1", voucher_no: "SINV-1", posting_date: "2026-05-03", party: "Acme", tax: "Sales tax", rate: "10", taxable: "100", tax_amount: "10", counted: "10" }],
          totals: { counted: "10" },
        });
      }),
    );
    expect(t).toContain("Documents in box OUT");
    expect(t).toContain("SINV-1");
    expect(t).toContain("Total in the box 10.00");
  });

  it("shows the server's refusal when the return cannot be opened, and waits while it loads", () => {
    expect(text(screen("/desk/tax-return/r1"))).toContain("Opening the return");
    const failed = text(screen("/desk/tax-return/r1", (qc) => qc.getQueryCache().build(qc, { queryKey: ["erp", "return", "r1"] }).setState({ status: "error", error: new Error("no such return"), fetchStatus: "idle" })));
    expect(failed).toContain("no such return");
  });
});

describe("the tax return's arithmetic and bodies", () => {
  it("asks for a reason for each box adjusted without one", () => {
    const r = ret();
    expect(needReason(r.boxes, { MAN: { adjustment: "3" } })).toEqual(["MAN"]);
    expect(needReason(r.boxes, { MAN: { adjustment: "3", adjustment_reason: "Margin scheme" } })).toEqual([]);
    expect(needReason(r.boxes, { MAN: { adjustment: "0" } })).toEqual([]);
  });

  it("sends the table whole with each box's id, its adjustment and reason", () => {
    expect(adjustBody(ret().boxes, { MAN: { adjustment: "3", adjustment_reason: " Margin scheme " } }).boxes[1]).toEqual({ id: "b-MAN", adjustment: "3", adjustment_reason: "Margin scheme" });
    expect(boxOf(ret().boxes[0], {}).adjustment).toBe("0");
  });

  it("knows which boxes drill down, what a value is, the period and the status", () => {
    expect(drillable(def.boxes, "OUT")).toBe(true);
    expect(drillable(def.boxes, "NET")).toBe(false);
    expect(filedValue(box("MAN", "0", { adjustment: "3" }))).toBe("3");
    expect(startBody({ definition: "d", company: "c", from: "2026-05-01" })).toEqual({ definition: "d", company: "c", from_date: "2026-05-01" });
    expect([0, 1, 2].map((docstatus) => status({ docstatus: docstatus as 0 | 1 | 2 }))).toEqual(["draft", "filed", "cancelled"]);
  });
});
