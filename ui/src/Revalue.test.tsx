// What the revaluation screen shows for the server's answer, rendered to HTML (no browser).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { Revalue } from "./Revalue";
import { bodyOf, type Draft } from "./revaluation";

const draft: Draft = {
  total_gain_loss: "600",
  reversal_date: "2026-05-01",
  rows: [
    { account: "a1", customer: "c1", currency: "eur", balance: "500", book_value: "45000", exchange_rate: "92", revalued: "46000", gain_loss: "1000" },
    { account: "a2", supplier: "s1", currency: "eur", balance: "-200", book_value: "-18000", exchange_rate: "92", revalued: "-18400", gain_loss: "-400" },
  ],
};

function screen(data: Draft | null, all = true) {
  const qc = new QueryClient();
  qc.setQueryData(["me"], { default_company: "co-1", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  const perms = { read: "all", write: "all", create: all ? "all" : "none", delete: "none", submit: all ? "all" : "none", cancel: "none", export: "all", import: "none" };
  qc.setQueryData(["meta"], { entities: [{ name: "ExchangeRevaluation", label: "Exchange Revaluation", label_plural: "Exchange Revaluations", kind: "document", submittable: true, permissions: perms }], modules: [], packages: [] });
  const body = bodyOf({ company: "co-1", date: "2026-04-30", reverse: true, reversalDate: "", account: null });
  if (data) qc.setQueryData(["erp", "revalue", body], data);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/?date=2026-04-30"]}>
        <Revalue />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the revaluation screen", () => {
  it("shows each balance with its rate and what it gains or loses, and the net", () => {
    const t = text(screen(draft));
    expect(t).toContain("500.00 45,000.00 92.00 46,000.00 1,000.00");
    expect(t).toContain("-200.00 -18,000.00 92.00 -18,400.00 -400.00");
    expect(t).toContain("Net gain 600.00");
    expect(t).toContain("2 balances.");
  });

  it("says plainly that the gain is not realised", () => {
    expect(text(screen(draft))).toContain("The gain is unrealised");
  });

  it("says when there is nothing to revalue", () => {
    expect(text(screen({ ...draft, rows: [], total_gain_loss: "0" }))).toContain("Nothing to revalue on this day");
  });

  it("lets someone who cannot post look, and says who posts it", () => {
    expect(text(screen(draft, false))).toContain("An Accounts Manager posts it.");
    expect(text(screen(draft, true))).not.toContain("An Accounts Manager posts it.");
  });

  it("waits while the server works the balances out", () => {
    expect(text(screen(null))).toContain("Working out the balances");
  });
});
