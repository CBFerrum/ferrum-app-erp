import { describe, expect, it } from "vitest";
import { destination, monthName, needing, type Overview, plural, row, say } from "./attention";

const overview: Overview = {
  meta: { as_of: "2026-11-01" },
  rows: [
    { key: "overdue_receivables", item: "Overdue receivables", count: 1, amount: "1000", oldest: "2026-10-15" },
    { key: "receivables_due_week", item: "Receivables due this week", count: 1, amount: "400", oldest: "2026-11-05" },
    { key: "overdue_payables", item: "Overdue payables", count: 0, amount: "0", oldest: null },
    { key: "unreconciled_lines", item: "Bank lines to reconcile", count: 14, oldest: "2026-10-01", detail: [{ id: "ba", name: "Main", count: 14 }] },
    { key: "drafts", item: "Drafts waiting to be submitted", count: 3, detail: [{ entity: "SalesInvoice", label: "Sales Invoices", count: 3 }] },
    { key: "open_months", item: "Months ended and not closed", count: 0, detail: [] },
    { key: "unclosed_years", item: "Fiscal years ended and not closed", count: 1, detail: [{ id: "fy", name: "2025", end: "2025-12-31" }] },
    { key: "cash", item: "Cash and bank", amount: "100", detail: [] },
  ],
};

describe("what needs attention", () => {
  it("is the rows with something in them, most pressing first", () => {
    expect(needing(overview).map((r) => r.key)).toEqual(["overdue_receivables", "unreconciled_lines", "drafts", "unclosed_years"]);
  });

  it("is nothing when everything is in order", () => {
    expect(needing({ meta: { as_of: "2026-11-01" }, rows: overview.rows.map((r) => ({ ...r, count: 0 })) })).toEqual([]);
  });

  it("is said in words, singular or plural", () => {
    expect(say(row(overview, "overdue_receivables")!)).toBe("1 overdue invoice to collect");
    expect(say(row(overview, "unreconciled_lines")!)).toBe("14 bank lines to reconcile");
    expect(say(row(overview, "drafts")!)).toBe("3 drafts waiting");
    expect(say({ key: "open_months", item: "x", count: 21 })).toBe("21 months not closed");
    expect(plural(1, "fiscal year")).toBe("1 fiscal year");
  });

  it("names a month", () => {
    expect(monthName("2026-10-01")).toBe("Oct 2026");
    expect(monthName("2025-01-01")).toBe("Jan 2025");
  });

  it("leads to the report or screen that deals with it", () => {
    expect(destination(row(overview, "overdue_receivables")!, "co", "2026-11-01")).toBe("/r/receivables?company=co&as_of=2026-11-01&run=1");
    expect(destination(row(overview, "overdue_payables")!, null, "2026-11-01")).toBe("/r/payables?as_of=2026-11-01&run=1");
    expect(destination(row(overview, "unreconciled_lines")!, "co", "2026-11-01")).toBe("/r/bank_reconciliation");
    expect(destination(row(overview, "drafts")!, "co", "2026-11-01")).toBeNull();
  });
});
