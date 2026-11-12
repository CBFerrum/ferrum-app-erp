// What the payment run desk shows for the answers of the Finance add-on (shapes as in
// addons/finance/tests/payment_run_it.rs), rendered to HTML (no browser), and what its pure parts decide.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";
import { PaymentRunDesk, PaymentRunNew } from "./PaymentRun";
import { createBody, failed, left, patchBody, problem, totals, type Run, type RunLine } from "./paymentRuns";

const line = (over: Partial<RunLine>): RunLine => ({ id: "l1", pay: true, supplier: "s1", supplier_name: "Acme", invoice_number: "BILL-1", invoice_type: "Purchase Invoice", due_date: "2026-05-01", outstanding: "1000", discount: "20", amount: "980", state: "Proposed", ...over });
const run = (over: Partial<Run>): Run => ({ id: "r1", version: 3, docstatus: 0, number: null, company: "co", posting_date: "2026-05-02", bank_account: "ba", due_by: "2026-05-10", currency_code: "EUR", total_amount: "980", paid_amount: "0", failed_count: 0, run_status: "Proposed", lines: [line({})], ...over });

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "all", cancel: "none", export: "all", import: "none" };
function screen(data: Run | null | { error: Error }, ui = <PaymentRunDesk />, installed = true) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: installed ? [{ name: "PaymentRun", label: "Payment Run", label_plural: "Payment Runs", kind: "document", submittable: true, permissions: perms }] : [], modules: [], packages: [] });
  qc.setQueryData(["erp", "run-approval", "r1"], null);
  if (data && "error" in data) qc.getQueryCache().build(qc, { queryKey: ["erp", "run", "r1"] }).setState({ status: "error", error: data.error, fetchStatus: "idle" });
  else if (data) qc.setQueryData(["erp", "run", "r1"], data);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/r1"]}>
        <Routes>
          <Route path="/:id" element={ui} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the payment run desk", () => {
  it("shows the proposal with its discount and the sums above it", () => {
    const t = text(screen(run({})));
    expect(t).toContain("To pay 980.00");
    expect(t).toContain("Discounts taken 20.00");
    expect(t).toContain("1,000.00");
    expect(t).toContain("Payment run (draft)");
    expect(t).toContain("Submit");
  });

  it("shows each failed line with the server's reason and offers to pay it again", () => {
    const failedRun = run({ docstatus: 1, number: "PRUN-2026-0001", run_status: "Partly Paid", failed_count: 1, lines: [line({ state: "Failed", error: "the supplier is blocked" })] });
    const t = text(screen(failedRun));
    expect(t).toContain("the supplier is blocked");
    expect(t).toContain("Pay the failed lines again");
    expect(t).toContain("Lines that failed 1");
  });

  it("offers nothing to pay again once every line is paid", () => {
    const t = text(screen(run({ docstatus: 1, run_status: "Paid", paid_amount: "980", lines: [line({ state: "Paid", payment_entry: "pe1" })] })));
    expect(t).not.toContain("Make payments");
    expect(t).toContain("Paid so far 980.00");
  });

  it("says plainly when the proposal is empty", () => {
    expect(text(screen(run({ lines: [], total_amount: "0" })))).toContain("Nothing to pay");
  });

  it("shows the server's plain refusal when the run cannot be opened", () => {
    expect(text(screen({ error: new Error("the run is not yours") }))).toContain("the run is not yours");
  });

  it("says when the add-on is not installed, and waits while the run loads", () => {
    expect(text(screen(run({}), <PaymentRunDesk />, false))).toContain("Not available here");
    expect(text(screen(null))).toContain("Opening the payment run");
  });

  it("starts a run from filters, asking for the bank account first", () => {
    const t = text(screen(null, <PaymentRunNew />));
    expect(t).toContain("Propose bills");
    expect(t).toContain("Pay from");
  });
});

describe("the payment run's arithmetic", () => {
  it("sums only the lines that pay, with the edits applied", () => {
    const lines = [line({}), line({ id: "l2", supplier: "s2", outstanding: "500", discount: "0", amount: "500" })];
    expect(totals(lines)).toMatchObject({ count: 2, suppliers: 2, pay: "1480", discount: "20" });
    expect(totals(lines, { l2: { pay: false } })).toMatchObject({ count: 1, pay: "980" });
    expect(totals(lines, { l1: { amount: "900" } }).pay).toBe("1400");
  });

  it("refuses what the server refuses: nothing to pay, and more than is outstanding with the discount", () => {
    expect(problem(line({ amount: "0" }))).toBe("nothing");
    expect(problem(line({ amount: "990" }))).toBe("over");
    expect(problem(line({ amount: "980" }))).toBeNull();
    expect(problem(line({ pay: false, amount: "0" }))).toBeNull();
  });

  it("says what stays open when a bill is paid in part", () => {
    expect(left(line({ amount: "500" }))).toBe("480");
  });

  it("sends the table whole with each line's id", () => {
    expect(patchBody([line({})], { l1: { amount: "950", instrument_no: "7" } })).toEqual({ lines: [{ id: "l1", pay: true, discount: "20", amount: "950", instrument_no: "7" }] });
  });

  it("leaves blank filters out of what makes the draft", () => {
    expect(createBody({ company: "co", posting_date: "2026-05-02", bank_account: "ba", due_by: "2026-05-10", take_discounts: true, supplier: "", minimum_amount: "" })).toEqual({
      company: "co",
      posting_date: "2026-05-02",
      bank_account: "ba",
      due_by: "2026-05-10",
      take_discounts: true,
    });
  });

  it("lists the lines that failed", () => {
    expect(failed([line({ state: "Failed" }), line({ id: "l2", state: "Paid" }), line({ id: "l3", state: "Failed", pay: false })])).toHaveLength(1);
  });
});
