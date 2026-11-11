// What the statements show for the server's answer (the finance tests' own February against January),
// rendered to HTML (no browser).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Statement } from "./Statement";
import type { Kind, Statement as Data } from "./statements";

const perms = { read: "all", write: "all", create: "all", delete: "all", submit: "none", cancel: "none", export: "all", import: "all" };
const meta = { entities: [{ name: "JournalEntry", label: "Journal Entry", label_plural: "Journal Entries", kind: "document", submittable: true, permissions: perms }], modules: [], packages: [] };

const columns = [
  { name: "account", label: "Account", type: "data" },
  { name: "amount", label: "Feb 2026", type: "currency" },
  { name: "amount_1", label: "Jan 2026", type: "currency" },
  { name: "change", label: "Change", type: "currency" },
  { name: "change_pct", label: "Change %", type: "percent" },
];

const profit: Data = {
  columns,
  rows: [
    { account: "Income", _bold: true },
    { account: "4110 - Sales", account_id: "sales", amount: "1000", amount_1: "3000", change: "-2000", change_pct: "-66.67", _indent: 1, _bold: false },
    { account: "Total Income", amount: "1000", amount_1: "3000", change: "-2000", change_pct: "-66.67", _bold: true },
    { account: "Expenses", _bold: true },
    { account: "5230 - Rent", account_id: "rent", amount: "600", amount_1: "500", change: "100", change_pct: "20", _indent: 1, _bold: false },
    { account: "Total Expenses", amount: "600", amount_1: "500", change: "100", change_pct: "20", _bold: true },
    { account: "Net Profit", amount: "400", amount_1: "2500", change: "-2100", change_pct: "-84", _bold: true },
  ],
  totals: { income: "1000", expense: "600", net_profit: "400" },
  meta: {
    periods: [
      { key: "amount", label: "Feb 2026", from_date: "2026-02-01", to_date: "2026-02-28", income: "1000", expense: "600", net_profit: "400" },
      { key: "amount_1", label: "Jan 2026", from_date: "2026-01-01", to_date: "2026-01-31", income: "3000", expense: "500", net_profit: "2500" },
    ],
  },
};

function screen(kind: Kind, search: string, data: Data | null, params: Record<string, string>) {
  const qc = new QueryClient();
  qc.setQueryData(["me"], { default_company: "co-1", fiscal_year_start_month: 4, user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], meta);
  if (data) qc.setQueryData(["erp", "statement", kind, params], data);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Statement kind={kind} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

const pl = "?from_date=2026-02-01&to_date=2026-02-28&company=co-1&compare=Previous%20period";
const plParams = { from_date: "2026-02-01", to_date: "2026-02-28", company: "co-1", compare: "Previous period", periods: "1" };

describe("the profit and loss", () => {
  it("shows each period in a column of its own, with how the first moved", () => {
    const t = text(screen("profit_and_loss", pl, profit, plParams));
    expect(t).toContain("Account Feb 2026 Jan 2026 Change");
    expect(t).toContain("4110 - Sales");
    expect(t).toContain("-2,000.00 (-66.67%)");
    expect(t).toContain("+100.00 (+20.00%)");
    expect(t).toContain("-2,100.00 (-84.00%)");
  });

  it("adds up what it found at the top, with the earlier period beside it", () => {
    const t = text(screen("profit_and_loss", pl, profit, plParams));
    expect(t).toContain("Net profit 400.00");
    expect(t).toContain("-2,100.00 from Jan 2026");
    expect(t).toContain("Income 1,000.00");
  });

  it("makes each account's figures buttons that open the entries behind them", () => {
    const html = screen("profit_and_loss", pl, profit, plParams);
    expect(html).toContain("Show the entries behind this figure");
    expect((html.match(/<button[^>]*erp-fig/g) ?? []).length).toBe(4);
  });

  it("tells headings, accounts, totals and the closing total apart, and marks the negative figures", () => {
    const html = screen("profit_and_loss", pl, profit, plParams);
    expect(html.match(/data-kind="section"/g)).toHaveLength(2);
    expect(html.match(/data-kind="total"/g)).toHaveLength(2);
    expect(html.match(/data-kind="grand"/g)).toHaveLength(1);
    expect(html).toContain("erp-neg");
  });

  it("names the statement, then says in one line what it covers", () => {
    const html = screen("profit_and_loss", pl, { ...profit, meta: { ...profit.meta, company: "Demo Industries", currency: "INR" } }, plParams);
    expect(html).toContain('<h1 class="page-title">Profit and loss</h1>');
    expect(text(html)).toContain("Demo Industries Feb 1, 2026 to Feb 28, 2026 INR");
  });

  it("can be told apart: statements, the period, what it is set beside", () => {
    const t = text(screen("profit_and_loss", pl, profit, plParams));
    expect(t).toContain("Profit and loss Balance sheet Cash flow");
    expect(t).toContain("Period");
    expect(t).toContain("Compare with");
    expect(t).toContain("Cost centre");
    expect(t).toContain("Download CSV");
  });

  it("says plainly when nothing was posted", () => {
    const empty: Data = { ...profit, columns: columns.slice(0, 2), rows: ["Income", "Total Income", "Expenses", "Total Expenses", "Net Profit"].map((account) => ({ account })), meta: { periods: profit.meta.periods.slice(0, 1) } };
    expect(text(screen("profit_and_loss", "?from_date=2026-02-01&to_date=2026-02-28&company=co-1", empty, { from_date: "2026-02-01", to_date: "2026-02-28", company: "co-1" }))).toContain("Nothing was posted in this period");
  });

  it("waits while it runs", () => {
    expect(text(screen("profit_and_loss", pl, null, plParams))).toContain("Running the statement");
  });
});

describe("the balance sheet and the cash flow", () => {
  it("shout when they do not add up", () => {
    const sheet: Data = {
      columns: columns.slice(0, 2),
      rows: [{ account: "Total Assets", amount: "100", _bold: true }],
      meta: { balanced: false, difference: "5", periods: [{ key: "amount", label: "28 Feb 2026", from_date: "2026-02-28", to_date: "2026-02-28", assets: "100", liabilities: "95", equity: "0" }] },
    };
    expect(text(screen("balance_sheet", "?as_of=2026-02-28&company=co-1", sheet, { as_of: "2026-02-28", company: "co-1" }))).toContain("out of balance by 5.00");
    const flow: Data = {
      columns: columns.slice(0, 2),
      rows: [{ account: "Net increase (decrease) in cash", amount: "-2100" }],
      meta: { reconciled: false, difference: "1", periods: [{ key: "amount", label: "Feb 2026", from_date: "2026-02-01", to_date: "2026-02-28", operating: "-600", investing: "0", financing: "-1500", net_change: "-2100" }] },
    };
    const t = text(screen("cash_flow", "?from_date=2026-02-01&to_date=2026-02-28&company=co-1", flow, { from_date: "2026-02-01", to_date: "2026-02-28", company: "co-1" }));
    expect(t).toContain("1.00 away from the bank and cash accounts");
    expect(t).toContain("Operating -600.00");
    expect(t).toContain("Net change in cash -2,100.00");
  });
});
