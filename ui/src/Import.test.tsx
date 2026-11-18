// What the statement import shows, rendered to HTML (no browser): the files it takes, the limit, the way
// in for someone who has only the text, and a file read with a statement format that fits it.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ImportDialog, { type Trial } from "./Import";

const words = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");

describe("importing a bank statement", () => {
  it("asks for the file first, names the files it reads, says how big it may be, and offers pasting", () => {
    const html = renderToString(
      <QueryClientProvider client={new QueryClient()}>
        <ImportDialog account="ba-1" onClose={() => undefined} onDone={() => undefined} />
      </QueryClientProvider>,
    );
    const text = words(html);
    expect(text).toContain("Import a bank statement");
    expect(text).toContain("Drop the statement here, or choose the file");
    expect(text).toContain("A statement file from your bank, up to 5 MB.");
    expect(text).toContain("Paste the text instead");
    expect(html).toContain('type="file"');
    expect(html).toContain('accept=".csv,.txt,.xml,.sta,.940,.mt940,.ofx,.qfx"');
  });

  it("says which format read the file, offers the others, and shows what the import would do", () => {
    const client = new QueryClient();
    const text = "<Document>camt.053</Document>";
    const fitted: Trial = {
      format: { id: "f-camt", name: "ISO 20022 statement (camt.053)" },
      imported: 2,
      duplicates: 1,
      skipped: 1,
      other_accounts: 0,
      errors: [],
      from_date: "2026-03-05",
      to_date: "2026-03-10",
      preview: [{ date: "2026-03-05", description: "Acme Corp Invoice INV-1001", reference: "INV-1001", deposit: "1500.00", withdrawal: "0.00", duplicate: true }],
    };
    client.setQueryData(["erp", "bank", "statement-read", "ba-1", "", text], fitted);
    client.setQueryData(["erp", "statement-formats"], { rows: [{ id: "f-camt", format_name: "ISO 20022 statement (camt.053)" }, { id: "f-ofx", format_name: "OFX and QFX" }] });
    const html = renderToString(
      <QueryClientProvider client={client}>
        <ImportDialog account="ba-1" onClose={() => undefined} onDone={() => undefined} initial={{ text, name: "march.xml" }} />
      </QueryClientProvider>,
    );
    const shown = words(html);
    expect(shown).toContain("Read as ISO 20022 statement (camt.053)");
    expect(shown).toContain("OFX and QFX");
    expect(shown).toContain("CSV, columns chosen here");
    expect(html).toContain('for="imp-format"');
    expect(shown).toContain("Already imported");
    expect(shown).not.toContain("Money in");
  });
});
