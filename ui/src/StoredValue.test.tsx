// The stored value desk for the answers of the kind's actions (src/stored_value.rs), rendered to
// HTML, and the bodies it sends: a code and a PIN go to `prove` and `balance` and to nothing else.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { StoredValue } from "./StoredValue";
import { balanceBody, carriesSecret, holdBody, issueBody, proveBody, running, topUpBody, type Kind } from "./storedValues";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
const kind: Kind = { id: "k1", kind_name: "Gift card", company: "co", holds: "Money", requires_pin: true, can_top_up: true, hold_hours: 72 };

function screen(search: string, data: Kind | null) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: false, roles: [] } });
  qc.setQueryData(["meta"], { entities: ["StoredValueKind", "StoredValueAccount"].map((name) => ({ name, label: name, label_plural: name, kind: "master", submittable: false, permissions: perms })), modules: [], packages: [] });
  if (data) qc.setQueryData(["erp", "sv-kind", "k1"], data);
  return renderToString(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/${search}`]}>
        <StoredValue />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ");

describe("the stored value desk", () => {
  it("asks for a kind first", () => {
    expect(text(screen("", null))).toContain("Choose a kind");
  });

  it("asks for the code and the PIN in boxes that never show them", () => {
    const html = screen("?kind=k1", kind);
    expect(html).toMatch(/<input[^>]*type="password"[^>]*id="sv-code"|<input[^>]*id="sv-code"[^>]*type="password"/);
    expect(html).toMatch(/type="password"[^>]*id="sv-pin"|id="sv-pin"[^>]*type="password"/);
    expect(text(html)).toContain("Look up");
  });

  it("asks for a PIN only where the kind needs one", () => {
    expect(screen("?kind=k1", { ...kind, requires_pin: false })).not.toContain("sv-pin");
  });

  it("the issue form says the code is shown once when left empty", () => {
    expect(text(screen("?kind=k1&tab=issue", kind))).toContain("Empty: the server makes one and shows it once.");
  });

  it("waits while the kind opens", () => {
    expect(text(screen("?kind=k1", null))).toContain("Opening the kind");
  });
});

describe("what the desk sends", () => {
  it("gives the code and PIN to prove and balance only", () => {
    expect(proveBody("issue", " 1234 ", "99", "k")).toEqual({ operation: "issue", key: "k", code: "1234", pin: "99" });
    expect(proveBody("issue", "", "", "k")).toEqual({ operation: "issue", key: "k" });
    expect(balanceBody("1234", "")).toEqual({ code: "1234" });
  });

  it("passes the proof, never the code, to what is logged", () => {
    for (const body of [issueBody({ amount: "50", reason: "goodwill", holder: null, proof: "p" }), holdBody({ order: "o", amount: "5", proof: "p" }), topUpBody({ amount: "5", reason: "x" })]) {
      expect(carriesSecret(body)).toBe(false);
    }
    expect(issueBody({ amount: "50", reason: "goodwill", holder: "c1", proof: "p" })).toEqual({ amount: "50", reason: "goodwill", holder: "c1", proof: "p" });
    expect(carriesSecret({ code: "1" })).toBe(true);
  });

  it("knows whether a hold is still running by the time the server gave", () => {
    const h = { hold: "h", account: "a", amount: "5", held: "5", available: "0", expires_at: "2026-05-05T10:00:00Z" };
    expect(running(h, Date.parse("2026-05-05T09:00:00Z"))).toBe(true);
    expect(running(h, Date.parse("2026-05-05T11:00:00Z"))).toBe(false);
  });
});
