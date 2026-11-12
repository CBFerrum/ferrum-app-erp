// The promotions desk: the report grouped by promotion (the answer's shape is src/reports.rs
// `promotions`), the empty basket, and what the basket sends and reads (src/promotion.rs `price_lines`).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";
import { Promotions } from "./Promotions";
import { basketBody, groupGiven, linesOf, rejectedCode, took, type Priced } from "./promoRules";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
function screen(path: string, report?: object) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: [{ name: "Promotion", label: "Promotion", label_plural: "Promotions", kind: "master", submittable: false, permissions: perms }], modules: [], packages: [] });
  if (report) qc.setQueryData(["erp", "promotions-given", "co", "", ""], report);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/desk/promotions/:id?" element={<Promotions />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

const given = [
  { promotion: "Spring", promotion_id: "p1", period: "2026-04", channel: "Web Shop", documents: 3, discount: "30" },
  { promotion: "Spring", promotion_id: "p1", period: "2026-05", channel: "Sales Order", documents: 1, discount: "12.50" },
  { promotion: "Autumn", promotion_id: "p2", period: "2026-05", channel: "Sales Order", documents: 2, discount: "8" },
];
const priced: Priced = {
  lines: [{ item: "i1", qty: "2", rate: "90", amount: "180" }],
  promotions: [
    { promotion: "p1", title: "Spring", line: 0, amount: "20", code: null },
    { promotion: "p1", title: "Spring", line: 1, amount: "5", code: "SPR" },
  ],
  rejected_codes: ["USEDUP", { code: "OLD", reason: "expired" }],
};

describe("the promotions desk", () => {
  it("asks for a promotion before showing one, and offers the basket", () => {
    const t = text(screen("/desk/promotions"));
    expect(t).toContain("No promotion chosen");
    expect(t).toContain("Test basket");
    expect(t).toContain("Price the basket");
  });

  it("lays the promotion's fields out in groups with the server's labels, each editable where it stands", () => {
    const f = (name: string, label: string, type: string, extra: object = {}) => ({ name, label, type, can_read: true, can_write: true, ...extra });
    const meta = {
      name: "Promotion", label: "Promotion", label_plural: "Promotions", kind: "master", submittable: false, permissions: perms, form: [], children: {}, workflow: null, actions: [],
      fields: [f("title", "Title", "data"), f("valid_from", "Valid From", "date"), f("gives", "Gives", "select", { options: ["Percent Off the Document"] }), f("percent", "Percent", "percent"), f("priority", "Priority", "int"), f("max_uses", "Uses Allowed", "int")],
    };
    const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
    qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
    qc.setQueryData(["meta"], { entities: [{ name: "Promotion", label: "Promotion", label_plural: "Promotions", kind: "master", submittable: false, permissions: perms }], modules: [], packages: [] });
    qc.setQueryData(["meta", "Promotion"], meta);
    qc.setQueryData(["peek", "Promotion", "p1"], { id: "p1", version: 1, title: "Spring", gives: "Percent Off the Document", percent: "10", priority: 5, max_uses: 100 });
    const html = renderToString(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/desk/promotions/p1"]}>
          <Routes>
            <Route path="/desk/promotions/:id?" element={<Promotions />} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );
    const t = text(html);
    for (const w of ["Schedule", "What it gives", "Limits", "Stacking", "Valid From", "Uses Allowed", "Spring"]) expect(t).toContain(w);
  });

  it("shows the Promotions Given report under each promotion with what it gave in all", () => {
    const t = text(screen("/desk/promotions?tab=given", { rows: given, totals: { discount: "50.5" } }));
    expect(t).toContain("Spring 42.50");
    expect(t).toContain("Autumn 8.00");
    expect(t).toContain("Total 50.50");
  });

  it("says when nothing was given", () => {
    expect(text(screen("/desk/promotions?tab=given", { rows: [], totals: { discount: "0" } }))).toContain("No promotion was given in this period");
  });
});

describe("the test basket", () => {
  it("sends the lines as JSON text, leaving out rows with no item or quantity and blank overrides", () => {
    const body = basketBody({ customer: "c1", rows: [{ item: "i1", qty: "2", rate: "", discount: "10" }, { item: null, qty: "1", rate: "", discount: "" }, { item: "i2", qty: "0", rate: "", discount: "" }], channel: " Web Shop ", place: null, at: "2026-05-05T10:30", codes: "SPR" });
    expect(body).toEqual({ customer: "c1", lines: JSON.stringify([{ item: "i1", qty: "2", discount_percent: "10" }]), channel: "Web Shop", at: "2026-05-05 10:30", codes: "SPR" });
  });

  it("leaves the moment out so the server uses the company's now", () => {
    expect(basketBody({ customer: "c", rows: [], channel: "", place: "w1", at: "", codes: "" })).toEqual({ customer: "c", lines: "[]", place: "w1" });
    expect(linesOf([])).toEqual([]);
  });

  it("says what one promotion took and where, or that it did not apply", () => {
    expect(took(priced, "p1")).toEqual({ amount: "25", lines: [0, 1] });
    expect(took(priced, "p2")).toBeNull();
    expect(took(priced, null)).toBeNull();
  });

  it("reads a rejected code bare or with its reason", () => {
    expect(priced.rejected_codes.map(rejectedCode)).toEqual([{ code: "USEDUP", reason: null }, { code: "OLD", reason: "expired" }]);
  });

  it("groups the report by promotion in the report's order", () => {
    const g = groupGiven(given);
    expect(g.map((x) => [x.promotion, x.discount, x.rows.length])).toEqual([["Spring", "42.5", 2], ["Autumn", "8", 1]]);
  });
});
