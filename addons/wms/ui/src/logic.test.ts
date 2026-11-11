import { describe, expect, it } from "vitest";
import { statusTone } from "@ferrum/kit";
import { BIN_TONES, byUrgency, cleanScan, COUNT_TONES, custodyWords, evidenceWords, INBOUND_TONES, lineProgress, loadScans, SHIPMENT_TONES, shortOf, STOCK_TONES, type Task, taskWords, unverified } from "./logic";

const task = (status: string, id = status): Task => ({ id, summary: "", kind: "Move", status, custody: null, qty: "1", reason: null, item: "X", lot: null, from_bin: null, to_bin: null, robot: null });

describe("what a task is, in words", () => {
  it("never lets arrival read as done", () => {
    expect(taskWords("Arrived").label).not.toMatch(/done|delivered/i);
    expect(taskWords("Arrived").says).toMatch(/nothing has moved until you do/i);
  });

  it("says uncertain stock is out of reach", () => {
    expect(custodyWords("Uncertain")).toMatch(/out of reach/);
    expect(custodyWords("In Place")).toBeNull();
    expect(custodyWords("In Transit", "Arrived")).not.toMatch(/on its way/i);
  });

  it("puts what is stuck first", () => {
    expect(byUrgency([task("Moving"), task("Open"), task("Exception"), task("Arrived")]).map((t) => t.status)).toEqual(["Exception", "Arrived", "Open", "Moving"]);
  });
});

describe("what proves a move", () => {
  it("asks for a unit's label and nothing else", () => {
    expect(loadScans({ unit: "T-100", item: "BOLT", lot: "L-1" }).map((s) => s.key)).toEqual(["unit_scan"]);
  });

  it("asks loose stock what it is, and its lot and serial where it has them", () => {
    expect(loadScans({ item: "BOLT" }).map((s) => s.key)).toEqual(["item_scan"]);
    expect(loadScans({ item: "BOLT", lot: "L-1", serial: "S-9" }).map((s) => s.key)).toEqual(["item_scan", "lot_scan", "serial_scan"]);
  });

  it("never lets a manager's word read as a scan", () => {
    expect(evidenceWords("Manual")).toMatch(/not scanned/);
    expect(evidenceWords("Scan")).toBeNull();
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

describe("the colour of a state", () => {
  it("comes from the stored state id and says what the state means", () => {
    expect(taskWords("Exception").tone).toBe("red");
    expect(taskWords("Arrived").tone).toBe("amber");
    expect(taskWords("Done").tone).toBe("green");
    expect(INBOUND_TONES.Discrepancy).toBe("amber");
    expect(INBOUND_TONES.Received).toBe("green");
    expect(SHIPMENT_TONES["Partially Handed Over"]).toBe("amber");
    expect(COUNT_TONES.Review).toBe("amber");
    expect(BIN_TONES.Blocked).toBe("red");
    expect(STOCK_TONES.Quarantine).toBe("amber");
  });

  it("is neutral for a state the map does not know, whatever its words", () => {
    expect(statusTone("Something new", INBOUND_TONES)).toBe("neutral");
    expect(statusTone("Not blocked", BIN_TONES)).toBe("neutral");
  });
});
