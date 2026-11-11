// What the accountant's screens share at their top: the page's title, one quiet line saying what
// it covers, and the actions that belong to the whole page (kit PageHeader's job, with the shell's title kept in step).

import type { ReactNode } from "react";
import { isolate } from "@ferrum/client";

/** The facts under a title, each its own run so the separators are drawn by the stylesheet. */
export function Meta({ items }: { items: (ReactNode | false | null | undefined | "")[] }) {
  return (
    <>
      {items.map((x, i) => (x ? <span key={i}>{x}</span> : null))}
    </>
  );
}

export function PageHead({ title, meta, actions }: { title: string; meta?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="erp-head">
      <div className="erp-titles">
        <h1 className="page-title">{isolate(title)}</h1>
        {meta && <p className="erp-meta">{meta}</p>}
      </div>
      {actions && <div className="erp-actions">{actions}</div>}
    </header>
  );
}

/** A control with its name beside it, small enough to sit in a row of them. */
export function Ctl({ label, htmlFor, title, children }: { label: string; htmlFor: string; title?: string; children: ReactNode }) {
  return (
    <div className="erp-ctl">
      <label className="erp-ctl-name" htmlFor={htmlFor} title={title}>
        {label}
      </label>
      {children}
    </div>
  );
}
