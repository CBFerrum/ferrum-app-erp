// The withholding desk for the answers of the categories and the `withholding` report (src/reports.rs),
// rendered to HTML, and its grouping.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { Withholding } from "./Withholding";
import { byCategory, byParty, type Category, forParty, inForce, type Line, statementCsv } from "./withholdings";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
const cat: Category = {
  id: "c1", category_name: "Services", applies: "On Bill", base: "Net Total", cumulative_base: "Above Threshold",
  rates: [{ valid_from: "2020-04-01", rate: "10", single_threshold: "30000", yearly_threshold: "100000" }, { valid_from: "2026-04-01", rate: "5", yearly_threshold: "120000" }, { valid_from: "2099-01-01", rate: "1" }],
  accounts: [{ company: "co", account: "a1" }],
};
const line = (over: Partial<Line> = {}): Line => ({ category: "Services", party: "Acme", posting_date: "2026-05-03", voucher_type: "PurchaseInvoice", voucher_no: "PINV-1", voucher_id: "v1", against: null, certificate: null, base: "1000", taxable: "1000", rate: "5", amount: "50", ...over });

function screen(search: string, seed: (qc: QueryClient) => void) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: [{ name: "WithholdingCategory", label: "Withholding Category", label_plural: "Withholding Categories", kind: "master", submittable: false, permissions: perms }], modules: [], packages: [] });
  seed(qc);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Withholding />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");
const withCats = (qc: QueryClient) => {
  qc.setQueryData(["erp", "wh-ids"], { rows: [{ id: "c1" }] });
  qc.setQueryData(["erp", "wh-category", "c1"], cat);
};

describe("the withholding desk", () => {
  it("shows each category with its dated rates and thresholds and which rate is in force", () => {
    vi.setSystemTime(new Date("2026-06-15T12:00:00Z"));
    const t = text(screen("", withCats));
    expect(t).toContain("Services");
    expect(t).toContain("Deducted from the bill");
    expect(t).toContain("only the part above the yearly threshold");
    expect(t).toContain("5.00");
    expect(t).toContain("120,000.00");
    expect(t).toContain("In force");
    vi.useRealTimers();
  });

  it("says plainly when there is no category", () => {
    expect(text(screen("", (qc) => qc.setQueryData(["erp", "wh-ids"], { rows: [] })))).toContain("No withholding category yet");
  });

  it("shows a statement per party with what each document withheld", () => {
    const t = text(screen("?tab=statement", (qc) => qc.setQueryData(["erp", "withheld", "co", "", ""], { rows: [line(), line({ voucher_no: "PINV-2", amount: "25", base: "500", taxable: "500" }), line({ party: "Beta", amount: "7" })], totals: { amount: "82" } })));
    expect(t).toContain("Acme 75.00");
    expect(t).toContain("Beta 7.00");
    expect(t).toContain("PINV-2");
    expect(t).toContain("Download the statement");
  });

  it("shows what is payable by category with the account it books to", () => {
    const t = text(
      screen("?tab=payable", (qc) => {
        withCats(qc);
        qc.setQueryData(["erp", "withheld", "co", "", ""], { rows: [line(), line({ amount: "25" })], totals: { amount: "75" } });
      }),
    );
    expect(t).toContain("To pay over");
    expect(t).toContain("Services");
    expect(t).toContain("75.00");
  });

  it("shows the server's refusal when the report cannot be run", () => {
    const t = text(screen("?tab=statement", (qc) => qc.getQueryCache().build(qc, { queryKey: ["erp", "withheld", "co", "", ""] }).setState({ status: "error", error: new Error("the period is too long"), fetchStatus: "idle" })));
    expect(t).toContain("the period is too long");
  });
});

describe("withholding's grouping", () => {
  it("takes the latest rate whose day has come", () => {
    expect(inForce(cat.rates, "2026-06-15")?.rate).toBe("5");
    expect(inForce(cat.rates, "2019-01-01")).toBeNull();
  });

  it("sums by category and by party in the report's order", () => {
    const rows = [line(), line({ party: "Beta", amount: "7" }), line({ category: "Rent", amount: "9" })];
    expect(byCategory(rows).map((s) => [s.key, s.amount])).toEqual([["Services", "57"], ["Rent", "9"]]);
    expect(byParty(rows).map((s) => [s.key, s.amount])).toEqual([["Acme", "59"], ["Beta", "7"]]);
    expect(forParty(rows, " be ")).toHaveLength(1);
  });

  it("writes the statement as a file, quoting what needs it and never letting a name act as a formula", () => {
    const csv = statementCsv([line({ party: '=cmd, "x"' })], ["Category", "Party"]);
    expect(csv.split("\r\n")[0]).toBe("Category,Party");
    expect(csv).toContain(`"'=cmd, ""x"""`);
  });
});
