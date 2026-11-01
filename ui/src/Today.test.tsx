// What the overview screen shows for the server's answer, rendered to HTML (no browser).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import type { Overview } from "./attention";
import { Today } from "./Today";

const overview: Overview = {
  meta: { as_of: "2026-11-01" },
  rows: [
    { key: "overdue_receivables", item: "Overdue receivables", count: 2, amount: "12500.5", oldest: "2026-10-15" },
    { key: "receivables_due_week", item: "Receivables due this week", count: 1, amount: "400", oldest: "2026-11-05" },
    { key: "overdue_payables", item: "Overdue payables", count: 0, amount: "0", oldest: null },
    { key: "payables_due_week", item: "Payables due this week", count: 0, amount: "0", oldest: null },
    { key: "unreconciled_lines", item: "Bank lines to reconcile", count: 14, oldest: "2026-10-01", detail: [{ id: "ba-1", name: "Main", count: 14, oldest: "2026-10-01" }] },
    { key: "drafts", item: "Drafts waiting to be submitted", count: 3, oldest: "2026-10-30", detail: [{ entity: "SalesInvoice", label: "Sales Invoices", count: 3, oldest: "2026-10-30" }] },
    { key: "open_months", item: "Months ended and not closed", count: 2, oldest: "2026-09-01", detail: [{ fiscal_year: "fy-1", start: "2026-10-01", end: "2026-10-31" }, { fiscal_year: "fy-1", start: "2026-09-01", end: "2026-09-30" }] },
    { key: "unclosed_years", item: "Fiscal years ended and not closed", count: 0, detail: [] },
    { key: "cash", item: "Cash and bank", amount: "1100", detail: [{ id: "a1", name: "1121 - Primary Bank", balance: "1000" }, { id: "a2", name: "1131 - Cash", balance: "100" }] },
  ],
};

function screen(data: Overview | null, roles: string[] = ["Accounts Manager"]) {
  const qc = new QueryClient();
  qc.setQueryData(["me"], { default_company: "co-1", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles } });
  if (data) qc.setQueryData(["erp", "overview", "co-1", "2026-11-01"], data);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/?as_of=2026-11-01"]}>
        <Today />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("accounts today", () => {
  it("says how many things need looking at", () => {
    // Overdue receivables, bank lines, drafts and months: four.
    expect(text(screen(overview))).toContain("4 things to look at.");
  });

  it("puts the overdue amounts first and says what is calm", () => {
    const t = text(screen(overview));
    expect(t).toContain("Owed to you 12,500.50 overdue: 2 invoices, the oldest due");
    expect(t).toContain("400.00 falls due this week (1 invoice)");
    expect(t).toContain("Owed by you Nothing is overdue.");
  });

  it("lists the bank accounts with lines left, and the drafts by kind", () => {
    const t = text(screen(overview));
    expect(t).toContain("14 lines of the bank statements are not reconciled");
    expect(t).toContain("Main 14");
    expect(t).toContain("3 drafts waiting to be submitted");
    expect(t).toContain("Sales Invoices 3");
  });

  it("lets an accounts manager close a month, and nobody else", () => {
    expect(text(screen(overview))).toContain("Oct 2026 Close");
    expect(text(screen(overview, ["Accounts User"]))).not.toContain("Close");
  });

  it("shows the cash", () => {
    expect(text(screen(overview))).toContain("1,100.00");
    expect(text(screen(overview))).toContain("1121 - Primary Bank 1,000.00");
  });

  it("is calm when there is nothing to do", () => {
    const quiet: Overview = { ...overview, rows: overview.rows.map((r) => (r.count ? { ...r, count: 0, detail: [] } : r)) };
    expect(text(screen(quiet))).toContain("Everything is in hand.");
  });

  it("waits while it looks", () => {
    expect(text(screen(null))).toContain("Looking at the books");
  });
});
