// Development only: ERP's own pages on fixture data, for looking at the layout in a browser when the
// test workspace has no bank account, no unapplied payments or no portal user. Served by the Vite dev server at
// /dev/erp-harness.html?s=bank|bank-empty|bank-done|allocate|revalue|needs-calm|portal-invoices|portal-invoice|portal-orders|portal-order
// (&dark=1 for the dark theme); it is not imported by the app and not an entry of the build, so it is not in a
// release. No server is called: fetch is answered here, and the figures are made up.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRoot } from "react-dom/client";
import { MemoryRouter, Route, Routes } from "react-router";
import { ConfirmHost, Toaster } from "@ferrum/kit";
import "../src/styles.css";
import { Needs } from "../src/home";
import { Bank } from "../src/Bank";
import { Allocate } from "../src/Allocate";
import { Revalue } from "../src/Revalue";
import { PortalInvoice, PortalInvoices } from "../src/Invoices";
import { PortalOrderPage, PortalOrders } from "../src/Orders";

const sp = new URLSearchParams(location.search);
const scenario = sp.get("s") ?? "bank";
if (sp.get("dark")) document.documentElement.dataset.theme = "dark";
if (sp.get("density")) document.documentElement.dataset.density = sp.get("density")!;
if (sp.get("rtl")) document.documentElement.dir = "rtl";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const day = (back: number) => new Date(Date.now() - back * 86_400_000).toISOString().slice(0, 10);
const perms = { read: "all", write: "all", create: "all", delete: "all", submit: "all", cancel: "all", export: "all", import: "all", actions: [] };
const ent = (name: string, submittable = false) => ({ name, label: name, label_plural: name + "s", kind: "document", module: "accounting", package: "accounting", submittable, permissions: perms });

// ── the bank ──
const m = (n: string, party: string, ref: string, back: number, amount: string, exact: boolean) => ({ payment_document: "PaymentEntry", payment_entry: `pe-${n}`, number: `PE-2026-${n}`, posting_date: day(back), amount, remaining: amount, party, reference: ref, exact_amount: exact, reference_match: exact, party_match: exact, days_apart: 1, score: "0.9" });
const line = (id: string, description: string, ref: string, back: number, amount: string, balance: string, match: ReturnType<typeof m> | null, status = "Unreconciled", unallocated = amount.replace("-", "")) => ({ id, number: `BT-${id}`, date: day(back), description, reference_number: ref, deposit: amount.startsWith("-") ? "0" : amount, withdrawal: amount.startsWith("-") ? amount.slice(1) : "0", amount, allocated: "0", unallocated: status === "Reconciled" ? "0" : unallocated, status, version: 1, balance, allocations: status === "Reconciled" ? [{ payment_document: "PaymentEntry", payment_entry: "pe-9", payment_number: "PE-2026-0009", posting_date: day(back), allocated_amount: amount.replace("-", "") }] : [], match });
const lines = scenario === "bank-empty" ? [] : [
  line("1", "NEFT Bharat Retail INV-2026-0014", "UTR4410982", 1, "33514.28", "143514.28", m("0031", "Bharat Retail", "INV-2026-0014", 2, "33514.28", true)),
  line("2", "Karnataka Components Ltd payment", "CHQ 004512", 2, "18000.00", "110000.00", m("0030", "Karnataka Components Ltd", "SO-2026-00002", 3, "18000.00", true)),
  line("3", "Bank charges Oct", "CHG-1022", 3, "-295.00", "92000.00", null),
  line("4", "Transfer from savings, a reference long enough that the line has to wrap onto a second row", "TRF 99120", 4, "50000.00", "92295.00", null, "Partially Reconciled", "20000.00"),
  line("5", "UPI Ganesh Traders", "UPI 8821", 5, "7250.50", "42295.00", m("0028", "Ganesh Traders, Pune", "", 9, "7000.00", false)),
  line("6", "Salary run October", "NEFT-SAL", 6, "-120000.00", "35044.50", null),
  ...(scenario === "bank-done" ? [] : [line("7", "Matched earlier: Nimbus Retail", "UTR1182", 8, "4400.00", "155044.50", null, "Reconciled")]),
];
const workbench = { bank_account: "ba1", name: "1121 - Primary Bank", currency: "INR", precision: 2, as_of: day(0), figures: { statement_balance: "143514.28", book_balance: "131000.00", difference: "12514.28", uncleared_deposits: "7000.00", uncleared_payments: "0.00", unrecorded: "5514.28", expected_bank_balance: "138000.00", unexplained: "5514.28" }, counts: { unreconciled: 6, matched: 1 }, lines, books: [{ payment_document: "PaymentEntry", payment_entry: "pe-0027", number: "PE-2026-0027", posting_date: day(7), party: "Nimbus Retail", reference: "SO-2026-00001", amount: "12000.00", remaining: "12000.00" }], truncated: false, books_truncated: false };

// ── applying payments ──
const open = (id: string, type: string, no: string, back: number, due: number, out: string) => ({ voucher_id: id, voucher_type: type, voucher_no: no, posting_date: day(back), due_date: day(due), currency: "INR", outstanding: out });
const receivables = { rows: [open("p1", "PaymentEntry", "PE-2026-0012", 12, 12, "-25000"), open("c1", "SalesInvoice", "CN-2026-0002", 9, 9, "-4000"), open("i1", "SalesInvoice", "INV-2026-0014", 40, 10, "21500.50"), open("i2", "SalesInvoice", "INV-2026-0021", 25, -5, "12013.78"), open("i3", "SalesInvoice", "INV-2026-0030", 6, -24, "9000")] };

// ── revaluing ──
const revalue = { rows: [{ account: "a1", customer: "c-usd", currency: "USD", balance: "4200.00", book_value: "347760.00", exchange_rate: "83.5100", revalued: "350742.00", gain_loss: "2982.00" }, { account: "a2", supplier: "s-eur", currency: "EUR", balance: "-1800.00", book_value: "-160200.00", exchange_rate: "90.4000", revalued: "-162720.00", gain_loss: "-2520.00" }, { account: "a3", currency: "USD", balance: "950.00", book_value: "78990.00", exchange_rate: "83.5100", revalued: "79334.50", gain_loss: "344.50" }], total_gain_loss: "806.50", reversal_date: day(-1) };

// ── the portal ──
const invoices = [
  { id: "i1", number: "INV-2026-0014", date: day(40), due_date: day(10), status: "Unpaid", customer: "Bharat Retail", currency: "INR", total: "21500.50", outstanding: "21500.50", overdue: true },
  { id: "i2", number: "INV-2026-0021", date: day(25), due_date: day(-5), status: "Unpaid", currency: "INR", total: "12013.78", outstanding: "12013.78", overdue: false },
  { id: "i3", number: "INV-2026-0009", date: day(70), due_date: day(40), status: "Partly Paid", currency: "INR", total: "30000.00", outstanding: "8000.00", overdue: true },
  { id: "i4", number: "INV-2026-0003", date: day(110), due_date: day(80), status: "Paid", currency: "INR", total: "9000.00", outstanding: "0", overdue: false },
];
const invoice = { ...invoices[0], company: "Demo Industries Pvt Ltd", pdf: true, lines: [{ item: "Steel bracket 40mm", description: "Zinc coated, box of 50", qty: "20", unit: "Box", rate: "700.00", amount: "14000.00" }, { item: "Hex bolt M8", qty: "500", unit: "Nos", rate: "12.50", amount: "6250.00" }], taxes: [{ description: "CGST 9%", amount: "1822.50" }, { description: "SGST 9%", amount: "1822.50" }], net_total: "20250.00", total_taxes: "3645.00", grand_total: "23895.00" };
const orders = [
  { id: "o1", number: "PO-2026-0042", date: day(14), required_by: day(-7), status: "To Receive and Bill", supplier: "Demo", currency: "INR", total: "88500.00", received: "40", billed: "0" },
  { id: "o2", number: "PO-2026-0038", date: day(30), required_by: day(5), status: "To Bill", currency: "INR", total: "15200.00", received: "100", billed: "0" },
  { id: "o3", number: "PO-2026-0031", date: day(60), status: "Completed", currency: "INR", total: "4300.00", received: "100", billed: "100" },
];
const bills = [{ id: "b1", bill_no: "SUP-118", bill_date: day(20), due_date: day(-10), status: "Unpaid", currency: "INR", total: "15200.00", owed: "15200.00", overdue: false }, { id: "b2", bill_no: "SUP-097", bill_date: day(60), due_date: day(30), status: "Unpaid", currency: "INR", total: "4300.00", owed: "4300.00", overdue: true }];
const order = { ...orders[0], company: "Demo Industries Pvt Ltd", pdf: true, lines: [{ item: "Steel bracket 40mm", description: "Zinc coated", qty: "100", received: "40", unit: "Box", rate: "700.00", amount: "70000.00", required_by: day(-7) }, { item: "Hex bolt M8", qty: "1000", received: "400", unit: "Nos", rate: "12.50", amount: "12500.00" }], taxes: [{ description: "GST 18%", amount: "13500.00" }], net_total: "82500.00", grand_total: "96000.00" };

const overview = (calm: boolean) => ({ meta: { as_of: day(0) }, rows: [{ key: "overdue_receivables", item: "x", count: calm ? 0 : 5, amount: "33514.28", oldest: day(26) }, { key: "overdue_payables", item: "x", count: 0, amount: "0" }, { key: "unreconciled_lines", item: "x", count: 0 }, { key: "drafts", item: "x", count: 0 }, { key: "open_months", item: "x", count: 0 }, { key: "unclosed_years", item: "x", count: 0 }] });

const real = window.fetch.bind(window);
window.fetch = async (input, init) => {
  const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url, location.href);
  const p = url.pathname;
  if (!p.startsWith("/api/")) return real(input, init);
  if (p === "/api/auth/me") return json({ default_company: "co-1", fiscal_year_start_month: 4, display_timezone: "UTC", timezone: "UTC", today: day(0), packages: [], scopes: {}, user: { id: "u1", email: "admin@demo.test", full_name: "Administrator", is_admin: true, roles: ["Accounts Manager"] } });
  if (p === "/api/meta") return json({ entities: ["BankAccount", "BankTransaction", "JournalEntry", "PaymentAllocation", "ExchangeRevaluation", "SalesInvoice", "Customer"].map((n) => ent(n)), modules: [], packages: [] });
  if (p.startsWith("/api/meta/")) return json({ name: p.split("/").pop(), label: p.split("/").pop(), fields: [], actions: [], title_field: null, permissions: perms });
  if (p === "/api/v1/BankAccount") return json({ rows: [{ id: "ba1", account_name: "1121 - Primary Bank", bank: "HDFC", masked_number: "••4521", is_default: true }] });
  if (p.endsWith("/actions/workbench")) return json(workbench);
  if (p.endsWith("/actions/get_suggestions")) {
    const l = lines.find((x) => p.includes(`/BankTransaction/${x.id}/`));
    return json({ suggestions: l?.match ? [l.match, m("0029", "Another Customer", "", 6, l.unallocated, false)] : [] });
  }
  if (p === "/api/reports/receivables") return json(receivables);
  if (p === "/api/reports/finance_overview") return json(overview(scenario === "needs-calm"));
  if (p === "/api/v1/ExchangeRevaluation/_calc") return json(revalue);
  if (p === "/api/v1/PaymentAllocation/_calc") return json({ allocations: [], total_allocated: "0", exchange_difference: "0" });
  if (p === "/api/app/selling/portal/invoices") return json({ invoices });
  if (p.startsWith("/api/app/selling/portal/invoices/")) return json(invoice);
  if (p === "/api/app/buying/portal/orders") return json({ orders });
  if (p === "/api/app/buying/portal/bills") return json({ bills });
  if (p === "/api/app/procurement/portal/rfqs") return json({ rfqs: [{ id: "r1", number: "RFQ-0007", due_by: day(-4), company: "Demo Industries Pvt Ltd", items: [{ item: "Steel bracket 40mm", qty: "200", unit: "Box" }], answer: "#" }] });
  if (p.startsWith("/api/app/buying/portal/orders/")) return json(order);
  if (/^\/api\/v1\/(Customer|Supplier|Company)(\/|$)/.test(p)) return json(p.split("/").length > 4 ? { id: p.split("/").pop(), title: decodeURIComponent(p.split("/").pop()!), number: decodeURIComponent(p.split("/").pop()!) } : { rows: [{ id: "Bharat Retail", number: "Bharat Retail" }, { id: "Karnataka Components Ltd", number: "Karnataka Components Ltd" }] });
  return json({ rows: [] });
};

const START: Record<string, string> = {
  "bank": "/bank?line=1", "bank-empty": "/bank", "bank-done": "/bank", allocate: "/allocate?party=Bharat%20Retail&company=co-1", revalue: "/revalue?company=co-1&date=" + day(11),
  "needs-calm": "/needs", "portal-invoices": "/portal-invoices", "portal-invoice": "/portal-invoice/i1", "portal-orders": "/portal-orders", "portal-order": "/portal-order/o1",
};
createRoot(document.getElementById("root")!).render(
  <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } } })}>
    <MemoryRouter initialEntries={[START[scenario] ?? "/bank"]}>
      <main className={scenario.startsWith("portal") ? "portal-page" : "p-4"} style={{ maxWidth: scenario.startsWith("portal") ? undefined : "72rem", margin: "0 auto", padding: "1.5rem" }}>
        <Routes>
          <Route path="/bank" element={<Bank />} />
          <Route path="/allocate" element={<Allocate />} />
          <Route path="/revalue" element={<Revalue />} />
          <Route path="/needs" element={<Needs />} />
          <Route path="/portal-invoices" element={<PortalInvoices />} />
          <Route path="/portal-invoice/:id" element={<PortalInvoice />} />
          <Route path="/portal-orders" element={<PortalOrders />} />
          <Route path="/portal-order/:id" element={<PortalOrderPage />} />
        </Routes>
      </main>
    </MemoryRouter>
    <Toaster />
    <ConfirmHost />
  </QueryClientProvider>,
);
