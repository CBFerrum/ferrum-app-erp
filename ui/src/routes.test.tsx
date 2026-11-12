// @vitest-environment jsdom
// The app's real route table, mounted: a new record and an existing one of each entity ERP gives a screen of its own,
// and of one it does not, must reach their page once the server has answered, never stay on the loading state (CRM's
// first release did: its screens handed the generic form an address that did not name the entity). The answers in
// src/fixtures are copied from a running server (GET /api/meta, /api/meta/<Entity>, /api/auth/me).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { createMemoryRouter, RouterProvider } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setFrameMode } from "@ferrum/records/frame";
import index from "./fixtures/meta-index.json";
import me from "./fixtures/me.json";
import { ERP_PACKAGES, erpRoutes } from "./routes";

const ENTITIES = ["Promotion", "MrpRun", "CarrierShipment", "PaymentRun", "NumberSeries", "StoredValueAccount", "TaxReturn", "Territory"];
const metas = import.meta.glob("./fixtures/meta-*.json", { eager: true, import: "default" }) as Record<string, unknown>;
const answers: Record<string, unknown> = { "/api/auth/me": me, "/api/meta": index };
for (const e of ENTITIES) {
  answers[`/api/meta/${e}`] = metas[`./fixtures/meta-${e}.json`];
  answers[`/api/v1/${e}/r1`] = { id: "r1", version: 1, docstatus: 0, name: "r1" };
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  (globalThis as { matchMedia?: unknown }).matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const path = new URL(String(input instanceof Request ? input.url : input), "http://localhost").pathname;
      const body = answers[path] ?? (path.startsWith("/api/v1/") ? { rows: [], total: 0 } : undefined);
      return new Response(JSON.stringify(body ?? { error: "not found" }), { status: body === undefined ? 404 : 200, headers: { "content-type": "application/json" } });
    }),
  );
  setFrameMode("app", ERP_PACKAGES, { pages: true });
});
afterEach(() => vi.unstubAllGlobals());

async function open(path: string) {
  const host = document.createElement("div");
  document.body.append(host);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createMemoryRouter(erpRoutes, { initialEntries: [path] });
  const root = createRoot(host);
  await act(async () => {
    root.render(
      <QueryClientProvider client={qc}>
        <RouterProvider router={router} />
      </QueryClientProvider>,
    );
  });
  for (let i = 0; i < 40; i++) await act(async () => void (await new Promise((r) => setTimeout(r, 25))));
  const text = host.textContent ?? "";
  await act(async () => root.unmount());
  host.remove();
  return text;
}

describe("ERP's routes as mounted", () => {
  for (const e of ENTITIES) {
    for (const id of ["new", "r1"]) {
      it(`/e/${e}/${id} leaves the loading state`, async () => {
        const text = await open(`/e/${e}/${id}`);
        expect(text.trim().length).toBeGreaterThan(0);
        expect(text).not.toMatch(/Loading|Opening (the|a|an) /);
      });
    }
  }

  it("the plain form of a taken-over record names its entity", async () => {
    expect(await open("/f/Promotion/new")).toContain("New Promotion");
  });
});
