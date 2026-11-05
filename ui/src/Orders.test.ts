import { describe, expect, it } from "vitest";
import { owedLabel, progress } from "./Orders";

describe("a supplier's orders", () => {
  it("say how far an order has come", () => {
    expect(progress({ received: "0", billed: "0", status: "To Receive and Bill" })).toBe("To deliver");
    expect(progress({ received: "40", billed: "0", status: "To Receive and Bill" })).toBe("40% received");
    expect(progress({ received: "100", billed: "0", status: "To Bill" })).toBe("Received, to bill");
    expect(progress({ received: "100", billed: "100", status: "Completed" })).toBe("Received and billed");
    expect(progress({ received: "40", billed: "0", status: "Closed" })).toBe("Closed");
  });

  it("say what is still owed on a bill", () => {
    expect(owedLabel({ owed: "0", overdue: false, due_date: "2026-11-30", is_return: false })).toBe("Paid");
    expect(owedLabel({ owed: "10", overdue: false, due_date: null, is_return: false })).toBe("To be paid");
    expect(owedLabel({ owed: "10", overdue: true, due_date: "2026-10-01", is_return: false })).toMatch(/^Overdue since /);
    expect(owedLabel({ owed: "10", overdue: false, due_date: "2026-11-30", is_return: false })).toMatch(/^Due /);
    expect(owedLabel({ owed: "-10", overdue: false, due_date: null, is_return: true })).toBe("Debit note");
  });
});
