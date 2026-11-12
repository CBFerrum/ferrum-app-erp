// The shipping desk for the answers of a shipment's actions (src/shipping.rs: rates are
// {service, amount, currency, days, id}), rendered to HTML, and what it offers in each state.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter, Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";
import { ShipmentDesk, ShippingHome } from "./Shipping";
import { cheapest, chooseBody, connectionOf, may, type Shipment } from "./shipments";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
const ship = (over: Partial<Shipment> = {}): Shipment => ({
  id: "s1", version: 1, number: "SHIP-0001", company: "co", carrier: "k1", reference: "DN-1", ship_to: "a1", status: "Draft",
  packages: [{ id: "p1", reference: "1 of 1", weight: "2.5", length: "30", width: "20", height: "10" }], ...over,
});
const rates = [{ service: "Express", amount: "21.5", currency: "EUR", days: 1 }, { service: "Ground", amount: "9", currency: "EUR", days: 4 }];

function screen(path: string, seed: (qc: QueryClient) => void) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: ["CarrierShipment", "CarrierConnection"].map((name) => ({ name, label: name, label_plural: name, kind: "document", submittable: false, permissions: perms })), modules: [], packages: [] });
  seed(qc);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/desk/shipping" element={<ShippingHome />} />
          <Route path="/desk/shipping/:id" element={<ShipmentDesk />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");
const one = (s: Shipment) => (qc: QueryClient) => qc.setQueryData(["erp", "shipment", "s1"], s);

describe("the shipping desk", () => {
  it("shows the parcels and offers to ask for rates", () => {
    const t = text(screen("/desk/shipping/s1", one(ship())));
    expect(t).toContain("1 of 1 2.50 30.00 20.00 10.00");
    expect(t).toContain("Get rates");
    expect(t).toContain("Choose a rate first.");
  });

  it("lists the carrier's answers, marks the cheapest and offers the label only after a rate is chosen", () => {
    const rated = text(screen("/desk/shipping/s1", one(ship({ status: "Rated", rates }))));
    expect(rated).toContain("Express · 21.50 EUR · 1 days");
    expect(rated).toContain("Ground · 9.00 EUR · 4 days · Cheapest");
    expect(rated).toContain("Choose this rate");
    expect(rated).toContain("Choose a rate first.");
    const html = screen("/desk/shipping/s1", one(ship({ status: "Rated", rates, service: "Ground" })));
    expect(text(html)).toContain("Ground is chosen.");
    expect(html.match(/<button[^>]*>[^<]*Buy the label/)?.[0]).not.toContain("disabled");
  });

  it("shows the tracking number, the label file and tracking once the label is bought, and no second buy", () => {
    const t = text(
      screen("/desk/shipping/s1", (qc) => {
        one(ship({ status: "Label Bought", tracking_number: "TRK1", tracking_url: "https://carrier.test/t/TRK1", label_file: "f1", cost: "9", tracking_status: "Picked up" }))(qc);
        qc.setQueryData(["erp", "shipment-files", "s1", "f1"], { attachments: [{ id: "f1", file_name: "label-TRK1.pdf", url: "/api/files/f1" }] });
      }),
    );
    expect(t).toContain("Tracking number TRK1");
    expect(t).toContain("Label file: label-TRK1.pdf");
    expect(t).toContain("Picked up");
    expect(t).toContain("Cancel the label");
    expect(t).not.toContain("Buy the label");
  });

  it("shows the server's plain refusal when a shipment cannot be opened", () => {
    const t = text(screen("/desk/shipping/s1", (qc) => qc.getQueryCache().build(qc, { queryKey: ["erp", "shipment", "s1"] }).setState({ status: "error", error: new Error("no such shipment"), fetchStatus: "idle" })));
    expect(t).toContain("no such shipment");
  });

  it("lists the carriers with the state of their platform connection, and a link to Administration", () => {
    const t = text(
      screen("/desk/shipping", (qc) => {
        qc.setQueryData(["erp", "shipments"], { rows: [{ id: "s1", number: "SHIP-0001", carrier: "k1", reference: "DN-1", service: "Ground", tracking_number: "TRK1", status: "In Transit" }] });
        qc.setQueryData(["erp", "carriers"], { rows: [{ id: "k1", connection_name: "Fast Freight", enabled: true, connection: "conn-1" }, { id: "k2", connection_name: "Slow", enabled: false, connection: null }] });
        qc.setQueryData(["erp", "platform-connections"], { connections: [{ id: "conn-1", name: "Fast Freight calls", enabled: true, last_error: "the carrier refused the key", health: { authentication: "failing", dead_letters: 2 } }] });
      }),
    );
    expect(t).toContain("SHIP-0001");
    expect(t).toContain("In transit");
    expect(t).toContain("Fast Freight calls");
    expect(t).toContain("Failing");
    expect(t).toContain("the carrier refused the key");
    expect(t).toContain("Not connected yet");
    expect(t).toContain("Connections in Administration");
  });
});

describe("what a shipment may do in each state", () => {
  it("follows the conditions of the entity's actions", () => {
    expect([may.rate(ship()), may.choose(ship()), may.buy(ship()), may.track(ship()), may.cancel(ship())]).toEqual([true, false, false, false, false]);
    const rated = ship({ status: "Rated", rates, service: "Ground" });
    expect([may.choose(rated), may.buy(rated)]).toEqual([true, true]);
    const bought = ship({ status: "Label Bought", tracking_number: "T" });
    expect([may.rate(bought), may.buy(bought), may.track(bought), may.cancel(bought)]).toEqual([false, false, true, true]);
    expect(may.cancel({ ...bought, status: "Delivered" })).toBe(false);
    expect(may.track({ ...bought, status: "Cancelled" })).toBe(false);
  });

  it("names the cheapest rate, finds a connection by id or name and sends the service chosen", () => {
    expect(cheapest(rates)?.service).toBe("Ground");
    expect(cheapest([])).toBeNull();
    const list = [{ id: "c1", name: "Fast", enabled: true }];
    expect(connectionOf(list, "c1")?.name).toBe("Fast");
    expect(connectionOf(list, "Fast")?.id).toBe("c1");
    expect(connectionOf(list, null)).toBeNull();
    expect(chooseBody("Ground")).toEqual({ service: "Ground" });
  });
});
