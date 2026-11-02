import { describe, expect, it } from "vitest";
import { coverageNote, cycleWarning, materialThere, materialWords, percent, remaining, runWords } from "./logic";

describe("what a figure can be called", () => {
  it("calls nothing to divide by unknown, not zero", () => {
    expect(percent(null)).toBe("Unknown");
    expect(percent(0)).toBe("0.0%");
    expect(percent(0.8)).toBe("80.0%");
  });

  it("says when part of the period was never seen", () => {
    expect(coverageNote(1)).toBeNull();
    expect(coverageNote(0.5)).toMatch(/50%/);
    expect(coverageNote(0.5)).toMatch(/not counted as running/);
    expect(coverageNote(null)).toMatch(/Nothing was planned/);
  });

  it("points at a wrong ideal cycle rather than hiding it", () => {
    expect(cycleWarning(1.4)).toMatch(/ideal cycle/);
    expect(cycleWarning(0.9)).toBeNull();
    expect(cycleWarning(null)).toBeNull();
  });
});

describe("a run", () => {
  it("is never ready to start with its material elsewhere", () => {
    expect(runWords("Released", "Waiting").label).toBe("Waiting for material");
    expect(runWords("Released", "Reserved").tone).toBe("amber");
    expect(runWords("Released", "Short").label).toBe("Material short");
    expect(runWords("Released", "At Station").label).toBe("Ready to start");
    expect(runWords("Released", "Preloaded").label).toBe("Ready to start");
    expect(materialThere("Reserved")).toBe(false);
    expect(materialWords("Reserved")).toMatch(/machine stays off/);
    expect(materialWords("At Station")).toBeNull();
  });

  it("counts down to nothing and no further", () => {
    expect(remaining("10", "4", "1")).toBe(5);
    expect(remaining("10", "12", null)).toBe(0);
  });

  it("calls a pause what it is", () => {
    expect(runWords("Paused").label).toBe("Stopped");
  });
});
