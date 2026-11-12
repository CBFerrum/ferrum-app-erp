// What a desk says when its add-on is absent, or when what it works from was never made (a bank account, a
// return definition, a promotion), and the numbering form's kind labels. Rendered to HTML with the answers seeded.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactElement } from "react";
import { renderToString } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";
import { NewSeries } from "./Numbering";
import { PaymentRunNew } from "./PaymentRun";
import { Promotions } from "./Promotions";
import { TaxReturnStart } from "./TaxReturn";

const perms = { read: "all", write: "all", create: "all", delete: "none", submit: "none", cancel: "none", export: "all", import: "none" };
const ent = (name: string, label: string) => ({ name, label, label_plural: label, kind: "master", submittable: false, permissions: perms });
const BANK = [["account_type", "in", ["Bank", "Cash"]], ["is_group", "=", false]];

function render(ui: ReactElement, opts: { admin?: boolean; entities?: string[]; seed?: (qc: QueryClient) => void }) {
  const qc = new QueryClient({ defaultOptions: { queries: { retryOnMount: false } } });
  qc.setQueryData(["me"], { default_company: "co", user: { id: "u", email: "a@b.test", full_name: "A B", is_admin: !!opts.admin, roles: [] } });
  const labels: Record<string, string> = { SalesOrder: "Sales Order", SalesInvoice: "Sales Invoice" };
  qc.setQueryData(["meta"], { entities: (opts.entities ?? []).map((n) => ent(n, labels[n] ?? n)), modules: [], packages: [] });
  qc.setQueryData(["meta", "NumberSeries"], { name: "NumberSeries", label: "Numbering Series", fields: [{ name: "document", label: "For", type: "select", options: ["SalesOrder", "SalesInvoice"], can_read: true, can_write: true }], form: [], children: {}, permissions: perms });
  opts.seed?.(qc);
  return renderToString(<QueryClientProvider client={qc}><MemoryRouter>{ui}</MemoryRouter></QueryClientProvider>).replace(/&amp;/g, "&");
}
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");

describe("a desk whose add-on is absent", () => {
  it("names the add-on and leads an administrator to the app store", () => {
    const html = render(<PaymentRunNew />, { admin: true });
    expect(text(html)).toContain("Not available here");
    expect(text(html)).toContain("comes with the Finance add-on");
    expect(html).toContain('href="/store/"');
  });

  it("tells anyone else to ask an administrator, with no link", () => {
    const html = render(<PaymentRunNew />, {});
    expect(text(html)).toContain("Ask an administrator to install it");
    expect(html).not.toContain("/store/");
  });
});

describe("a desk with nothing to work from", () => {
  it("payment run: no bank or cash account says so and leads to making one, not to a refused run", () => {
    const html = render(<PaymentRunNew />, { entities: ["PaymentRun"], seed: (qc) => qc.setQueryData(["erp", "any", "Account", BANK], { rows: [] }) });
    expect(text(html)).toContain("No bank or cash account yet");
    expect(html).toContain('href="/e/Account/new"');
    expect(text(html)).not.toContain("Propose bills");
  });

  it("payment run: with an account the form shows", () => {
    const html = render(<PaymentRunNew />, { entities: ["PaymentRun"], seed: (qc) => qc.setQueryData(["erp", "any", "Account", BANK], { rows: [{ id: "a1" }] }) });
    expect(text(html)).toContain("Propose bills");
  });

  it("tax return: no definition leads to making one", () => {
    const html = render(<TaxReturnStart />, { entities: ["TaxReturn"], seed: (qc) => qc.setQueryData(["erp", "any", "TaxReturnDefinition", null], { rows: [] }) });
    expect(text(html)).toContain("No tax return defined yet");
    expect(html).toContain('href="/e/TaxReturnDefinition/new"');
    expect(text(html)).not.toContain("Work out the return");
  });

  it("promotions: none saved leads to making one, not to a test basket over nothing", () => {
    const html = render(<Promotions />, { entities: ["Promotion"], seed: (qc) => qc.setQueryData(["erp", "any", "Promotion", null], { rows: [] }) });
    expect(text(html)).toContain("No promotion yet");
    expect(html).toContain('href="/f/Promotion/new"');
    expect(text(html)).not.toContain("Price the basket");
  });
});

describe("the new numbering series", () => {
  it("offers each document kind by its label from the metadata, not its type name", () => {
    const html = render(<NewSeries />, { entities: ["NumberSeries", "SalesOrder", "SalesInvoice"] });
    expect(html).toContain(">Sales Order</option>");
    expect(html).toContain(">Sales Invoice</option>");
    expect(html).not.toContain(">SalesOrder</option>");
  });
});
