// How the desks are reached: the routes they take over, the strip between a record's desk and its form,
// and the Create menu's entries in app.yaml.

import { readFileSync } from "node:fs";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { DeskTabs } from "./DeskTabs";
import { DESKS, formPath, isDesk, listPath, recordPath } from "./desks";

const main = readFileSync(new URL("./main.tsx", import.meta.url), "utf8");
const app = readFileSync(new URL("../../app.yaml", import.meta.url), "utf8");

describe("the desks' routes", () => {
  it("every record-bound desk takes over its record's page, and every list desk its list", () => {
    for (const d of DESKS) {
      const path = d.kind === "record" ? `"e/${d.entity}/:id"` : `"e/${d.entity}"`;
      expect(main, `${d.entity} ${d.kind}`).toContain(`path: ${path}`);
    }
  });

  it("keeps the plain form and list reachable, and sends a new record of a taken-over page to the form", () => {
    expect(main).toContain('path: "f/:entity/:id"');
    expect(main).toContain('path: "l/:entity"');
    for (const d of DESKS.filter((x) => x.kind === "record" && ["CarrierShipment", "Promotion", "MrpRun"].includes(x.entity))) expect(main).toContain(`<Navigate to="/f/${d.entity}/new" replace />`);
  });

  it("knows what is a desk and where the pages are", () => {
    expect(isDesk("PaymentRun", "record")).toBe(true);
    expect(isDesk("PaymentRun", "list")).toBe(false);
    expect(isDesk("Item", "record")).toBe(false);
    expect([recordPath("TaxReturn", "r1"), formPath("TaxReturn", "r1"), listPath("Promotion")]).toEqual(["/e/TaxReturn/r1", "/f/TaxReturn/r1", "/l/Promotion"]);
  });
});

describe("the strip between a desk and the record's form", () => {
  const html = (current: "desk" | "form") => renderToString(<MemoryRouter><DeskTabs entity="PaymentRun" id="r1" current={current} /></MemoryRouter>);
  it("links both pages and marks the one shown", () => {
    const h = html("desk");
    expect(h).toContain('href="/e/PaymentRun/r1"');
    expect(h).toContain('href="/f/PaymentRun/r1"');
    expect(h).toMatch(/aria-current="page"[^>]*>Desk|href="\/e\/PaymentRun\/r1"[^>]*aria-current="page"|aria-current="page"[^>]*href="\/e\/PaymentRun\/r1"/);
    expect(html("form")).not.toEqual(h);
  });
});

describe("the Create menu", () => {
  it("names each entry's entity, so only people who may create it see it, and a route the interface answers", () => {
    const entries = [...app.matchAll(/\{id: (\w+), label: ([^,]+), path: erp\/(\S+), entity: (\w+)\}/g)];
    expect(entries.length).toBe(4);
    for (const [, , , path, entity] of entries) {
      expect(path).toContain(`e/${entity}/new`);
      expect(main).toContain(`"e/${entity}/new"`);
    }
  });
});
