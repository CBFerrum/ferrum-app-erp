import { describe, expect, it } from "vitest";
import { amount, dueLabel } from "./Invoices";

describe("a customer's invoices", () => {
  it("say what is still to pay", () => {
    expect(dueLabel({ outstanding: "0", overdue: false, due_date: "2026-11-30", is_return: false })).toBe("Paid");
    expect(dueLabel({ outstanding: "10", overdue: false, due_date: null, is_return: false })).toBe("Due");
    expect(dueLabel({ outstanding: "10", overdue: true, due_date: "2026-10-01", is_return: false })).toMatch(/^Overdue since /);
    expect(dueLabel({ outstanding: "10", overdue: false, due_date: "2026-11-30", is_return: false })).toMatch(/^Due /);
    expect(dueLabel({ outstanding: "-10", overdue: false, due_date: null, is_return: true })).toBe("Credit note");
  });

  it("show amounts with their currency", () => {
    expect(amount("1770", "INR")).toMatch(/^INR 1,?770\.00$/);
    expect(amount("12.5", null)).toBe("12.50");
  });
});
