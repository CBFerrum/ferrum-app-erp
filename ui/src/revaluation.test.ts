import { describe, expect, it } from "vitest";
import { bodyOf, defaultDay, outcome, party } from "./revaluation";

describe("the day a revaluation runs at", () => {
  it("is the last day of the month before, unless today is a month's last day", () => {
    expect(defaultDay("2026-11-01")).toBe("2026-10-31");
    expect(defaultDay("2026-11-17")).toBe("2026-10-31");
    expect(defaultDay("2026-10-31")).toBe("2026-10-31");
    expect(defaultDay("2026-03-01")).toBe("2026-02-28");
    expect(defaultDay("2024-03-01")).toBe("2024-02-29");
    expect(defaultDay("2026-01-05")).toBe("2025-12-31");
    expect(defaultDay("2026-02-28")).toBe("2026-02-28");
  });
});

describe("the document asked about", () => {
  it("names the company, the day, and when it is reversed", () => {
    expect(bodyOf({ company: "co", date: "2026-10-31", reverse: true, reversalDate: "2026-11-01", account: null })).toEqual({
      company: "co",
      posting_date: "2026-10-31",
      auto_reverse: true,
      reversal_date: "2026-11-01",
      gain_loss_account: undefined,
    });
  });

  it("leaves the reversal day out when it is not reversed, and names a chosen account", () => {
    expect(bodyOf({ company: "co", date: "2026-10-31", reverse: false, reversalDate: "2026-11-01", account: "acc" })).toMatchObject({ auto_reverse: false, reversal_date: undefined, gain_loss_account: "acc" });
  });
});

describe("what it comes to", () => {
  it("is a gain, a loss, or nothing", () => {
    expect(outcome("600")).toBe("gain");
    expect(outcome("-300.5")).toBe("loss");
    expect(outcome("0")).toBe("none");
  });

  it("names the party of a balance", () => {
    expect(party({ customer: "c", supplier: null })).toEqual({ to: "Customer", id: "c" });
    expect(party({ customer: null, supplier: "s" })).toEqual({ to: "Supplier", id: "s" });
    expect(party({ customer: null, supplier: null })).toBeNull();
  });
});
