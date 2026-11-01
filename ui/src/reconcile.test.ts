import { describe, expect, it } from "vitest";
import {
  canReconcile,
  clearsLine,
  csvHeaders,
  enterTarget,
  exactMatches,
  guessMapping,
  leftToReconcile,
  mappingBody,
  mappingProblem,
  others,
  problems,
  reconcileBody,
  sniffDelimiter,
  step,
  tick,
  untick,
  why,
  type Book,
  type Line,
  type Match,
} from "./reconcile";

const match = (over: Partial<Match> = {}): Match => ({
  payment_document: "PaymentEntry",
  payment_entry: "pe-1",
  posting_date: "2026-03-02",
  amount: "1500",
  remaining: "1500",
  exact_amount: true,
  reference_match: false,
  party_match: false,
  days_apart: 0,
  score: "120",
  ...over,
});

const line = (id: string, amount: string, over: Partial<Line> = {}): Line => ({
  id,
  date: "2026-03-02",
  deposit: amount.startsWith("-") ? "0" : amount,
  withdrawal: amount.startsWith("-") ? amount.slice(1) : "0",
  amount,
  allocated: "0",
  unallocated: amount.replace("-", ""),
  status: "Unreconciled",
  version: 1,
  balance: null,
  allocations: [],
  match: null,
  ...over,
});

describe("moving through the statement", () => {
  const lines = [line("a", "10"), line("b", "20"), line("c", "30")];

  it("goes down and up, and stops at the ends", () => {
    expect(step(lines, "a", 1)).toBe("b");
    expect(step(lines, "c", 1)).toBe("c");
    expect(step(lines, "b", -1)).toBe("a");
    expect(step(lines, "a", -1)).toBe("a");
  });

  it("starts at the top going down and at the bottom going up when nothing is selected", () => {
    expect(step(lines, null, 1)).toBe("a");
    expect(step(lines, "gone", -1)).toBe("c");
    expect(step([], "a", 1)).toBeNull();
  });
});

describe("accepting with one key", () => {
  it("takes an exact match and nothing else", () => {
    expect(enterTarget(line("a", "1500", { match: match() }))?.payment_entry).toBe("pe-1");
    expect(enterTarget(line("a", "1500", { match: match({ exact_amount: false }) }))).toBeNull();
    expect(enterTarget(line("a", "1500"))).toBeNull();
    expect(enterTarget(undefined)).toBeNull();
  });

  it("adds up what accepting all exact matches would settle, in and out apart", () => {
    // 1500 + 800 in; 500 + 3.50 out; the 25 has no match.
    const lines = [
      line("1", "1500", { match: match() }),
      line("2", "800", { match: match({ payment_entry: "pe-2" }) }),
      line("3", "-500", { match: match({ payment_entry: "pe-3" }) }),
      line("4", "-25"),
      line("5", "-3.5", { match: match({ payment_entry: "pe-5" }) }),
    ];
    const all = exactMatches(lines);
    expect(all.lines.map((l) => l.id)).toEqual(["1", "2", "3", "5"]);
    expect([all.money_in, all.money_out]).toEqual(["2300", "503.5"]);
  });

  it("says why an entry was put forward", () => {
    expect(why(match({ reference_match: true, party_match: true, days_apart: 0 }))).toEqual(["Exact amount", "Same reference", "Party named", "Same day"]);
    expect(why(match({ exact_amount: false, days_apart: 1 }))).toEqual(["1 day apart"]);
    expect(why(match({ exact_amount: false, days_apart: 12 }))).toEqual(["12 days apart"]);
  });
});

describe("ticking several entries against one line", () => {
  const entries = [
    { id: "x", remaining: "300", kind: "PaymentEntry" },
    { id: "y", remaining: "250.50", kind: "JournalEntry" },
  ];

  it("fills in what the line can still take", () => {
    // A line of 500: the first entry takes 300, the second only the 200 that is left of the line.
    let ticks = tick({}, entries[0], "500");
    expect(ticks).toEqual({ x: "300" });
    ticks = tick(ticks, entries[1], "500");
    expect(ticks).toEqual({ x: "300", y: "200" });
    expect(leftToReconcile("500", ticks)).toBe("0");
    expect(clearsLine("500", ticks)).toBe(true);
  });

  it("lets go of an entry and gives its share back", () => {
    const ticks = untick({ x: "300", y: "200" }, "x");
    expect(ticks).toEqual({ y: "200" });
    expect(leftToReconcile("500", ticks)).toBe("300");
    expect(clearsLine("500", ticks)).toBe(false);
  });

  it("is exact where floats are not", () => {
    // 0.1 + 0.2 against a line of 0.3.
    expect(leftToReconcile("0.3", { a: "0.1", b: "0.2" })).toBe("0");
  });

  it("refuses what cannot work, in words", () => {
    const over = problems({ x: "400" }, entries, "500");
    expect(over.amounts.x).toBe("Only 300 of this is left to clear");
    const more = problems({ x: "300", y: "250.5" }, entries, "500");
    expect(more.total).toBe("That is more than the 500 left on the line");
    expect(problems({ x: "" }, entries, "500").amounts.x).toBe("Enter an amount above zero");
    expect(problems({ x: "-1" }, entries, "500").amounts.x).toBe("Enter an amount above zero");
    const fine = problems({ x: "300", y: "200" }, entries, "500");
    expect(canReconcile({ x: "300", y: "200" }, fine)).toBe(true);
    expect(canReconcile({}, problems({}, entries, "500"))).toBe(false);
  });

  it("builds the body the server's Reconcile takes", () => {
    expect(reconcileBody({ x: "300.00", y: "200" }, entries)).toEqual({
      vouchers: [
        { payment_document: "PaymentEntry", payment_entry: "x", allocated_amount: "300" },
        { payment_document: "JournalEntry", payment_entry: "y", allocated_amount: "200" },
      ],
    });
  });

  it("offers the rest of the books in the line's direction, without what is put forward already", () => {
    const books: Book[] = [
      { payment_document: "PaymentEntry", payment_entry: "in-1", posting_date: "2026-03-01", amount: "250", remaining: "250" },
      { payment_document: "PaymentEntry", payment_entry: "out-1", posting_date: "2026-03-01", amount: "-300", remaining: "300" },
      { payment_document: "PaymentEntry", payment_entry: "out-2", posting_date: "2026-03-02", amount: "-500", remaining: "500" },
    ];
    expect(others(books, { amount: "-25" }, new Set(["out-2"])).map((b) => b.payment_entry)).toEqual(["out-1"]);
    expect(others(books, { amount: "10" }, new Set()).map((b) => b.payment_entry)).toEqual(["in-1"]);
  });
});

describe("reading a statement file", () => {
  it("finds the delimiter outside quotes", () => {
    expect(sniffDelimiter("Date,Description,Amount")).toBe(",");
    expect(sniffDelimiter("Txn Date;Narration;Debit;Credit")).toBe(";");
    expect(sniffDelimiter('"Date, posted"\t"Text"\t"Amount"')).toBe("\t");
  });

  it("reads the header row, quotes and all", () => {
    expect(csvHeaders('Date,"Description, long",Amount\n05/03/2026,x,1')).toEqual({ headers: ["Date", "Description, long", "Amount"], delimiter: "," });
    // A bank's file with a title line above the columns: the header row is the second.
    expect(csvHeaders("Statement for 12345\nDate;Ref\n1;2", 2)).toEqual({ headers: ["Date", "Ref"], delimiter: ";" });
    expect(csvHeaders("").headers).toEqual([""]);
  });

  it("guesses the columns of a statement with a signed amount", () => {
    const m = guessMapping(["Date", "Description", "Reference", "Amount"]);
    expect(m).toEqual({ date: "Date", description: "Description", reference: "Reference", amount: "Amount", deposit: "", withdrawal: "" });
    expect(mappingProblem(m)).toBeNull();
    expect(mappingBody(m)).toEqual({ date: "Date", description: "Description", reference: "Reference", amount: "Amount" });
  });

  it("guesses the columns of one with money in and out apart", () => {
    const m = guessMapping(["Txn Date", "Narration", "Chq/Ref No", "Debit", "Credit", "Balance"]);
    expect(m.date).toBe("Txn Date");
    expect(m.description).toBe("Narration");
    expect(m.reference).toBe("Chq/Ref No");
    expect([m.withdrawal, m.deposit, m.amount]).toEqual(["Debit", "Credit", ""]);
  });

  it("says what is missing before asking the server", () => {
    expect(mappingProblem(guessMapping(["Foo", "Bar"]))).toBe("Choose the column that holds the date.");
    expect(mappingProblem(guessMapping(["Date", "Narration"]))).toBe("Choose the amount column, or one column for money in and one for money out.");
  });
});
