// What the allocation screen shows for the receivables report's answer, rendered to HTML (no browser).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Allocate } from "./Allocate";

const row = (id: string, no: string, type: string, posted: string, outstanding: string, over: object = {}) => ({
  voucher_id: id,
  voucher_no: no,
  voucher_type: type,
  posting_date: posted,
  due_date: null,
  currency: "INR",
  outstanding,
  ...over,
});

function screen(search: string, rows: ReturnType<typeof row>[] | null) {
  const qc = new QueryClient();
  qc.setQueryData(["me"], { default_company: "co-1", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  if (rows) qc.setQueryData(["erp", "allocate", "open", "Customer", "acme", "co-1"], { rows });
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Allocate />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the payment allocation screen", () => {
  it("asks for a party first", () => {
    expect(text(screen("", null))).toContain("Choose a customer");
  });

  it("shows the money and the invoices side by side with the sums above", () => {
    const t = text(
      screen("?party=acme", [
        row("a", "SINV-1", "SalesInvoice", "2026-03-01", "1000", { due_date: "2026-03-31" }),
        row("b", "SINV-2", "SalesInvoice", "2026-03-10", "600"),
        row("p1", "PAY-1", "PaymentEntry", "2026-03-25", "-1200"),
        row("je", "JV-1", "JournalEntry", "2026-03-26", "-80"),
        row("cn", "CN-1", "SalesInvoice", "2026-03-27", "-150"),
      ]),
    );
    expect(t).toContain("Money to apply 1,430.00");
    expect(t).toContain("Applying 0.00");
    expect(t).toContain("Left over 1,430.00");
    expect(t).toContain("Payment");
    expect(t).toContain("Journal credit");
    expect(t).toContain("Credit note");
    expect(t).toContain("SINV-1");
    expect(t).toContain("1,000.00");
    expect(t).toContain("Oldest invoices first");
  });

  it("says when there is no money to apply, and where to record it", () => {
    const t = text(screen("?party=acme", [row("a", "SINV-1", "SalesInvoice", "2026-03-01", "1000")]));
    expect(t).toContain("No money waiting to be applied");
    expect(t).toContain("Record a payment");
  });

  it("says when there are no invoices to apply it to", () => {
    expect(text(screen("?party=acme", [row("p1", "PAY-1", "PaymentEntry", "2026-03-25", "-1200")]))).toContain("No open invoices to apply it to");
  });

  it("waits while what is open loads", () => {
    expect(text(screen("?party=acme", null))).toContain("Loading what is open");
  });
});
