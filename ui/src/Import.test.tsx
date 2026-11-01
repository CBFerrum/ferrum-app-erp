// What the statement import first shows, rendered to HTML (no browser): the file to drop, the
// limit, and the way in for someone who has only the text.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToString } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ImportDialog from "./Import";

describe("importing a bank statement", () => {
  it("asks for the file first, says how big it may be, and offers pasting", () => {
    const html = renderToString(
      <QueryClientProvider client={new QueryClient()}>
        <ImportDialog account="ba-1" onClose={() => undefined} onDone={() => undefined} />
      </QueryClientProvider>,
    );
    const text = html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");
    expect(text).toContain("Import a bank statement");
    expect(text).toContain("Drop the statement here, or choose the file");
    expect(text).toContain("up to 5 MB");
    expect(text).toContain("Paste the text instead");
    expect(html).toContain('type="file"');
  });
});
