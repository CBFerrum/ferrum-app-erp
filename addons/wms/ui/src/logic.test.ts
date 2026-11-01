import { describe, expect, it } from "vitest";
import { byUrgency, cleanScan, custodyWords, lineProgress, shortOf, type Task, taskWords, unverified } from "./logic";

const task = (status: string, id = status): Task => ({ id, summary: "", kind: "Move", status, custody: null, qty: "1", reason: null, item: "X", lot: null, from_bin: null, to_bin: null, robot: null });

describe("what a task is, in words", () => {
  it("never lets arrival read as done", () => {
    expect(taskWords("Arrived").label).not.toMatch(/done|delivered/i);
    expect(taskWords("Arrived").says).toMatch(/nothing has moved until you do/i);
  });

  it("says uncertain stock is out of reach", () => {
    expect(custodyWords("Uncertain")).toMatch(/out of reach/);
    expect(custodyWords("In Place")).toBeNull();
  });

  it("puts what is stuck first", () => {
    expect(byUrgency([task("Moving"), task("Open"), task("Exception"), task("Arrived")]).map((t) => t.status)).toEqual(["Exception", "Arrived", "Open", "Moving"]);
  });
});

describe("what a scanner sends", () => {
  it("loses the spaces and newline a scanner may add", () => {
    expect(cleanScan("  A-01\n")).toBe("A-01");
  });
});

describe("quantities", () => {
  it("knows a short pick", () => {
    expect(shortOf("90", "88")).toBe(true);
    expect(shortOf("90", "90")).toBe(false);
  });

  it("counts damaged goods as received, and shows anything over", () => {
    expect(lineProgress({ id: "1", item: "x", expected_qty: "100", received_qty: "96", damaged_qty: "4" })).toEqual({ got: 100, left: 0, over: false });
    expect(lineProgress({ id: "1", item: "x", expected_qty: "10", received_qty: "12" }).over).toBe(true);
  });

  it("knows what is still to verify", () => {
    expect(unverified([{ id: "a", item: "x", qty: "2", verified_qty: "2" }, { id: "b", item: "y", qty: "2", verified_qty: "1" }]).map((l) => l.id)).toEqual(["b"]);
  });
});
