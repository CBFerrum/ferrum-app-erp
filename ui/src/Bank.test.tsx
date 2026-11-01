// What the workbench shows for an answer of the server's, rendered to HTML (no browser): the shape
// of the answer is the one the finance tests get from the real action.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Bank } from "./Bank";
import type { Line, Match, Workbench } from "./reconcile";

const perms = { read: "all", write: "all", create: "all", delete: "all", submit: "none", cancel: "none", export: "all", import: "all" };
const meta = {
  entities: [
    { name: "BankAccount", label: "Bank Account", label_plural: "Bank Accounts", kind: "master", submittable: false, permissions: perms },
    { name: "BankTransaction", label: "Bank Transaction", label_plural: "Bank Transactions", kind: "document", submittable: true, permissions: perms },
  ],
  modules: [],
  packages: ["finance"],
};

const match: Match = { amount: "1500", days_apart: 0, exact_amount: true, number: "PAY-2026-00001", party: "Acme Corp", party_match: true, payment_document: "PaymentEntry", payment_entry: "pe-1", posting_date: "2026-03-02", reference: "INV-1", reference_match: true, remaining: "1500", score: "190" };
const line = (id: string, over: Partial<Line>): Line => ({
  id,
  date: "2026-03-02",
  amount: "0",
  unallocated: "0",
  allocated: "0",
  allocations: [],
  deposit: "0",
  withdrawal: "0",
  status: "Unreconciled",
  version: 2,
  match: null,
  balance: null,
  ...over,
});

const workbench: Workbench = {
  bank_account: "ba-1",
  name: "Main",
  currency: "INR",
  precision: 2,
  as_of: "2026-03-31",
  figures: { book_balance: "1743", difference: "25", expected_bank_balance: "1768", statement_balance: "1768", uncleared_deposits: "2550", uncleared_payments: "807", unexplained: "0", unrecorded: "1768" },
  counts: { matched: 1, unreconciled: 2 },
  books: [{ payment_document: "PaymentEntry", payment_entry: "pe-4", number: "PAY-2026-00004", posting_date: "2026-03-30", party: "Acme Corp", reference: "ACME-9", amount: "250", remaining: "250" }],
  books_truncated: false,
  truncated: false,
  lines: [
    line("l-1", { amount: "1500", deposit: "1500", unallocated: "1500", balance: "1500", date: "2026-03-02", description: "Payment from Acme Corp", reference_number: "INV-1", number: "BTXN-2026-000001", match }),
    line("l-2", { amount: "-25", withdrawal: "25", unallocated: "25", balance: "1475", date: "2026-03-05", description: "Bank charges", reference_number: null, number: "BTXN-2026-000004" }),
  ],
};

function screen(search: string, data: Workbench | null = workbench) {
  const qc = new QueryClient();
  qc.setQueryData(["meta"], meta);
  qc.setQueryData(["erp", "bank", "accounts"], [{ id: "ba-1", account_name: "Main", masked_number: "•••• 5432", is_default: true }]);
  if (data) qc.setQueryData(["erp", "bank", "workbench", "ba-1", "2026-03-31", false], data);
  qc.setQueryData(["erp", "bank", "suggestions", "l-1", 2], { suggestions: [match] });
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <Bank />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the bank reconciliation workbench", () => {
  it("shows the bank, the books and the difference between them, whatever line is selected", () => {
    const t = text(screen("?account=ba-1&as_of=2026-03-31&line=l-1"));
    expect(t).toContain("Bank statement 1,768.00");
    expect(t).toContain("Your books 1,743.00");
    expect(t).toContain("Difference 25.00");
    expect(t).toContain("All of it is explained below");
  });

  it("lists the statement lines with what each is, the bank's balance after it, and the exact matches", () => {
    const t = text(screen("?account=ba-1&as_of=2026-03-31&line=l-1"));
    expect(t).toContain("Payment from Acme Corp");
    expect(t).toContain("1,500.00");
    expect(t).toContain("-25.00");
    expect(t).toContain("Bank balance 1,475.00");
    expect(t).toContain("Match found");
    expect(t).toContain("2 lines to reconcile, 1 with an exact match");
    expect(t).toContain("Accept 1 exact match");
  });

  it("offers the exact match of the selected line for one key, with the reasons it was put forward", () => {
    const t = text(screen("?account=ba-1&as_of=2026-03-31&line=l-1"));
    expect(t).toContain("PAY-2026-00001 clears this line exactly");
    expect(t).toContain("Enter");
    expect(t).toContain("Exact amount");
    expect(t).toContain("Same reference");
    expect(t).toContain("Party named");
  });

  it("says what keys do", () => {
    expect(text(screen("?account=ba-1&as_of=2026-03-31&line=l-1"))).toContain("accept every exact match");
  });

  it("says plainly when everything is matched", () => {
    const t = text(screen("?account=ba-1&as_of=2026-03-31", { ...workbench, lines: [], counts: { matched: 0, unreconciled: 0 } }));
    expect(t).toContain("All caught up");
    expect(t).toContain("Nothing left to reconcile");
  });

  it("waits for the workbench while it loads", () => {
    expect(text(screen("?account=ba-1&as_of=2026-03-31", null))).toContain("Loading the statement");
  });
});
