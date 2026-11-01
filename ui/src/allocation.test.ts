import { describe, expect, it } from "vitest";
import { currencies, dueWords, documentBody, fill, fromReport, kindLabel, type Open, pairOff, postingDate, ready, split, tally } from "./allocation";

const item = (id: string, outstanding: string, date: string, over: Partial<Open> = {}): Open => ({ id, kind: outstanding.startsWith("-") ? "PaymentEntry" : "SalesInvoice", number: id.toUpperCase(), date, due: null, currency: "INR", outstanding, ...over });

// Acme: 1200 received on the 25th, a journal credit of 80 and a credit note of 150; owed 1000, 600 and 400.
const items = [
  item("a", "1000", "2026-03-01"),
  item("b", "600", "2026-03-10"),
  item("c", "400", "2026-03-20"),
  item("p1", "-1200", "2026-03-25", { number: "PAY-1" }),
  item("je", "-80", "2026-03-26", { kind: "JournalEntry", number: "JV-1" }),
  item("cn", "-150", "2026-03-27", { kind: "SalesInvoice", number: "CN-1" }),
];

describe("a party's open items", () => {
  it("come from the report, without what is settled", () => {
    const rows = [
      { voucher_id: "x", voucher_type: "SalesInvoice", voucher_no: "S-1", posting_date: "2026-03-01", due_date: "2026-03-31", currency: "INR", outstanding: "100" },
      { voucher_id: "y", voucher_type: "SalesInvoice", voucher_no: "S-2", posting_date: "2026-03-02", due_date: null, currency: "INR", outstanding: "0" },
    ];
    expect(fromReport(rows)).toEqual([{ id: "x", kind: "SalesInvoice", number: "S-1", date: "2026-03-01", due: "2026-03-31", currency: "INR", outstanding: "100" }]);
  });

  it("split into money to apply, oldest first, and invoices, the earliest due first", () => {
    const { credits, invoices } = split(items, "INR");
    expect(credits.map((c) => c.id)).toEqual(["p1", "je", "cn"]);
    expect(invoices.map((i) => i.id)).toEqual(["a", "b", "c"]);
    const due = split([item("late", "5", "2026-01-01", { due: "2026-06-01" }), item("soon", "5", "2026-02-01", { due: "2026-03-01" })], "INR");
    expect(due.invoices.map((i) => i.id)).toEqual(["soon", "late"]);
  });

  it("put the currencies that can be joined first", () => {
    const mixed = [...items, item("e1", "-100", "2026-03-01", { currency: "EUR" }), item("e2", "50", "2026-03-02", { currency: "EUR" }), item("u1", "30", "2026-03-02", { currency: "USD" })];
    expect(currencies(mixed)).toEqual(["INR", "EUR", "USD"]);
    expect(currencies(items)).toEqual(["INR"]);
  });

  it("are called what they are", () => {
    expect(items.map((i) => kindLabel(i))).toEqual(["Invoice", "Invoice", "Invoice", "Payment", "Journal credit", "Credit note"]);
  });
});

describe("applying money, oldest first", () => {
  const { credits, invoices } = split(items, "INR");

  it("fills the oldest invoices with all the money there is", () => {
    // 1200 + 80 + 150 = 1430: A takes 1000, B the 430 that is left, C nothing.
    expect(fill(credits, invoices)).toEqual({ a: "1000", b: "430" });
  });

  it("pairs the money in order, splitting a payment across invoices", () => {
    const { pairings, short } = pairOff(credits, invoices, { a: "1000", b: "430" });
    // A takes 1000 of the 1200; B takes the 200 left of it, then the 80, then 150 of the credit note.
    expect(pairings).toEqual([
      { credit: "p1", invoice: "a", amount: "1000" },
      { credit: "p1", invoice: "b", amount: "200" },
      { credit: "je", invoice: "b", amount: "80" },
      { credit: "cn", invoice: "b", amount: "150" },
    ]);
    expect(short).toBe("0");
  });

  it("says what is left over on each side", () => {
    const t = tally(credits, invoices, { a: "1000", b: "430" });
    expect([t.available, t.applied, t.left]).toEqual(["1430", "1430", "0"]);
    expect(t.invoiceLeft).toEqual({ a: "0", b: "170", c: "400" });
    expect(t.creditLeft).toEqual({ p1: "0", je: "0", cn: "0" });
    expect(ready(t)).toBe(true);
  });

  it("leaves money over when the invoices take less", () => {
    const t = tally(credits, invoices, { a: "1000" });
    expect([t.applied, t.left]).toEqual(["1000", "430"]);
    expect(t.creditLeft).toEqual({ p1: "200", je: "80", cn: "150" });
  });

  it("leaves an unused payment alone when it is left out", () => {
    const without = credits.filter((c) => c.id !== "p1");
    const t = tally(without, invoices, fill(without, invoices));
    // Without the 1200 there is 230 to apply: it all goes to A.
    expect([t.available, t.applied, t.left]).toEqual(["230", "230", "0"]);
    expect(t.invoiceLeft.a).toBe("770");
  });

  it("refuses more than the money there is, and more than an invoice is owed", () => {
    const over = tally(credits, invoices, { a: "1000", b: "600", c: "400" });
    expect(over.over).toBe("That is 570 more than the money there is to apply");
    expect(ready(over)).toBe(false);
    const toMuch = tally(credits, invoices, { a: "1000.01" });
    expect(toMuch.errors.a).toBe("Only 1000 is owed on this");
    expect(ready(toMuch)).toBe(false);
    expect(tally(credits, invoices, { a: "abc" }).errors.a).toBe("Enter an amount, or leave it empty");
    expect(ready(tally(credits, invoices, {}))).toBe(false);
  });

  it("adds cents exactly", () => {
    const small = [item("p", "-0.3", "2026-03-01"), item("i1", "0.1", "2026-03-01"), item("i2", "0.2", "2026-03-02")];
    const s = split(small, "INR");
    const t = tally(s.credits, s.invoices, fill(s.credits, s.invoices));
    expect([t.applied, t.left]).toEqual(["0.3", "0"]);
  });
});

describe("the document made of it", () => {
  it("is dated today, or later when a document of the pairings is", () => {
    expect(postingDate("2026-03-28", ["2026-03-25", "2026-03-27"])).toBe("2026-03-28");
    expect(postingDate("2026-03-20", ["2026-03-25"])).toBe("2026-03-25");
  });

  it("names the pairings the way the server's Payment Allocation takes them", () => {
    expect(documentBody({ company: "co", partyType: "Customer", party: "acme", date: "2026-03-28", pairings: [{ credit: "p1", invoice: "a", amount: "1000" }] })).toEqual({
      company: "co",
      party_type: "Customer",
      customer: "acme",
      posting_date: "2026-03-28",
      remarks: undefined,
      allocations: [{ credit: "p1", invoice: "a", allocated_amount: "1000" }],
    });
    expect(documentBody({ company: "co", partyType: "Supplier", party: "s", date: "d", pairings: [] })).toMatchObject({ supplier: "s" });
  });
});

describe("how late an invoice is", () => {
  it("is said in days", () => {
    expect(dueWords("2026-03-25", "2026-03-28")).toBe("3 days overdue");
    expect(dueWords("2026-03-27", "2026-03-28")).toBe("1 day overdue");
    expect(dueWords("2026-03-28", "2026-03-28")).toBe("Due today");
    expect(dueWords("2026-03-30", "2026-03-28")).toBe("Due in 2 days");
    expect(dueWords("2026-03-29", "2026-03-28")).toBe("Due in 1 day");
    expect(dueWords(null, "2026-03-28")).toBe("");
  });
});
