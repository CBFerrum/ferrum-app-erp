import { describe, expect, it } from "vitest";
import { abs, add, cmp, isDecimal, isZero, max, min, neg, normalize, sub, sum } from "./decimal";

describe("exact decimals", () => {
  it("add what floats cannot", () => {
    expect(add("0.1", "0.2")).toBe("0.3");
    expect(sub("1500", "1499.99")).toBe("0.01");
    expect(add("1234567890123456789.12", "0.88")).toBe("1234567890123456790");
    expect(sum(["3.5", "3.5", "-7"])).toBe("0");
  });

  it("keep the sign straight around zero", () => {
    expect(sub("1.5", "2")).toBe("-0.5");
    expect(neg("0")).toBe("0");
    expect(add("-0.5", "0.5")).toBe("0");
    expect(abs("-12.30")).toBe("12.3");
  });

  it("write numbers the way the server does", () => {
    expect(normalize("1500.50")).toBe("1500.5");
    expect(normalize("+007.0")).toBe("7");
    expect(normalize("-0.00")).toBe("0");
    expect(normalize("")).toBe("0");
    expect(normalize("not a number")).toBe("0");
  });

  it("compare across different lengths", () => {
    expect(cmp("2", "10")).toBe(-1);
    expect(cmp("0.10", "0.1")).toBe(0);
    expect(cmp("-1", "-1.0001")).toBe(1);
    expect(min("2.50", "2.5001")).toBe("2.5");
    expect(max("-3", "-4")).toBe("-3");
    expect(isZero("0.000")).toBe(true);
  });

  it("know what is a decimal", () => {
    expect(isDecimal("12.5")).toBe(true);
    expect(isDecimal("12,5")).toBe(false);
    expect(isDecimal(12.5)).toBe(false);
  });
});
