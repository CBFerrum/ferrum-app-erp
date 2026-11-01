import { describe, expect, it } from "vitest";
import { amountColumns, days, fiscalYearStart, ledgerHref, ledgerQuery, moved, named, periodOf, presets, toCsv } from "./statements";

describe("the periods a statement is asked for by name", () => {
  it("count a fiscal year that starts in April", () => {
    const p = Object.fromEntries(presets("2026-11-01", 4).map((x) => [x.id, [x.from, x.to]]));
    expect(p["this-month"]).toEqual(["2026-11-01", "2026-11-30"]);
    expect(p["last-month"]).toEqual(["2026-10-01", "2026-10-31"]);
    // November is the second month of the third quarter (October to December).
    expect(p["this-quarter"]).toEqual(["2026-10-01", "2026-12-31"]);
    expect(p["last-quarter"]).toEqual(["2026-07-01", "2026-09-30"]);
    expect(p["this-year"]).toEqual(["2026-04-01", "2027-03-31"]);
    expect(p["last-year"]).toEqual(["2025-04-01", "2026-03-31"]);
  });

  it("find the year that began in the year before", () => {
    const p = Object.fromEntries(presets("2026-02-10", 4).map((x) => [x.id, [x.from, x.to]]));
    expect(p["this-year"]).toEqual(["2025-04-01", "2026-03-31"]);
    expect(p["this-quarter"]).toEqual(["2026-01-01", "2026-03-31"]);
    expect(p["last-quarter"]).toEqual(["2025-10-01", "2025-12-31"]);
    expect(p["last-year"]).toEqual(["2024-04-01", "2025-03-31"]);
    expect(p["this-month"]).toEqual(["2026-02-01", "2026-02-28"]);
  });

  it("count a calendar year, and a leap February", () => {
    const p = Object.fromEntries(presets("2024-02-10", 1).map((x) => [x.id, [x.from, x.to]]));
    expect(p["this-month"]).toEqual(["2024-02-01", "2024-02-29"]);
    expect(p["this-quarter"]).toEqual(["2024-01-01", "2024-03-31"]);
    expect(p["last-month"]).toEqual(["2024-01-01", "2024-01-31"]);
    expect(p["this-year"]).toEqual(["2024-01-01", "2024-12-31"]);
    expect(p["last-quarter"]).toEqual(["2023-10-01", "2023-12-31"]);
  });

  it("name the days a balance sheet is taken at", () => {
    expect(days("2026-11-01", 4).map((d) => [d.label, d.to])).toEqual([
      ["Today", "2026-11-01"],
      ["End of last month", "2026-10-31"],
      ["End of last quarter", "2026-09-30"],
      ["End of last fiscal year", "2026-03-31"],
    ]);
  });

  it("know when a stretch is one of them, and when it is the person's own", () => {
    const list = presets("2026-11-01", 4);
    expect(named(list, "2026-10-01", "2026-10-31")).toBe("last-month");
    expect(named(list, "2026-10-02", "2026-10-31")).toBe("custom");
    expect(fiscalYearStart("2026-03-31", 4)).toBe("2025-04-01");
    expect(fiscalYearStart("2026-04-01", 4)).toBe("2026-04-01");
  });
});

describe("opening the entries behind a figure", () => {
  const drill = { company: "co", account: "rent", from: "2026-02-01", to: "2026-02-28", excludeClosing: true, costCenter: null, project: "web" };

  it("asks the general ledger for the account in the period, without the closing, with the dimensions", () => {
    expect(ledgerQuery(drill)).toEqual({ company: "co", account: "rent", from_date: "2026-02-01", to_date: "2026-02-28", exclude_closing: "true", project: "web" });
    expect(ledgerQuery({ ...drill, excludeClosing: false, project: null })).toEqual({ company: "co", account: "rent", from_date: "2026-02-01", to_date: "2026-02-28" });
  });

  it("links to the report, run", () => {
    expect(ledgerHref(drill)).toBe("/r/general_ledger?company=co&account=rent&from_date=2026-02-01&to_date=2026-02-28&exclude_closing=true&project=web&run=1");
  });

  it("opens a balance sheet figure from the start of its year to its day, so the closing is the figure", () => {
    const info = { key: "amount_1", label: "31 Jan 2026", from_date: "2026-01-31", to_date: "2026-01-31" };
    expect(periodOf("balance_sheet", info, 4)).toEqual({ from: "2025-04-01", to: "2026-01-31" });
    expect(periodOf("profit_and_loss", { ...info, from_date: "2026-01-01" }, 4)).toEqual({ from: "2026-01-01", to: "2026-01-31" });
  });

  it("knows which columns hold an amount", () => {
    const cols = [{ name: "account", label: "Account" }, { name: "amount", label: "Feb" }, { name: "amount_1", label: "Jan" }, { name: "change", label: "Change" }];
    expect(amountColumns(cols).map((c) => c.name)).toEqual(["amount", "amount_1"]);
  });
});

describe("how a figure moved", () => {
  const show = (v: string) => v;
  it("always shows the sign", () => {
    expect(moved("-2000", "-66.67", show)).toBe("-2000 (-66.67%)");
    expect(moved("100", "20", show)).toBe("+100 (+20%)");
    expect(moved("0", "0", show)).toBe("0 (0%)");
  });

  it("says nothing of a share of nothing", () => {
    expect(moved("500", null, show)).toBe("+500");
    expect(moved(undefined, undefined, show)).toBe("");
  });
});

describe("the file", () => {
  it("keeps the amounts as the server wrote them, indents the accounts, and quotes", () => {
    const csv = toCsv({
      columns: [{ name: "account", label: "Account" }, { name: "amount", label: "Feb 2026" }],
      rows: [
        { account: "Income", _bold: true },
        { account: "4110 - Sales, goods", amount: "1000.50", _indent: 1 },
        { account: "Net", amount: "-84" },
      ],
    });
    expect(csv).toBe('Account,Feb 2026\nIncome,\n"4110 - Sales, goods",1000.50\nNet,-84');
  });

  it("does not let a cell act as a formula", () => {
    expect(toCsv({ columns: [{ name: "account", label: "A" }], rows: [{ account: "=SUM(A1)" }, { account: "@x" }, { account: "-5" }] })).toBe("A\n'=SUM(A1)\n'@x\n-5");
  });
});
