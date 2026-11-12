// Numbering series (src/series.rs, packages/accounting/entities/masters.yaml `NumberSeries`): series
// per kind of document, which one is the default of its kind and company, and which series a document
// took. Pure. A kind is the entity's id; its name comes from the metadata.

export interface Series {
  id: string;
  version: number;
  series_name: string;
  document: string;
  company?: string | null;
  template: string;
  gapless?: boolean;
  use_by_default?: boolean;
  disabled?: boolean;
}

export interface Group {
  kind: string;
  series: Series[];
}

/** The series of each kind, kinds in the order the metadata offers them, series by name; kinds with none are left out. */
export function byKind(rows: Series[], order: string[]): Group[] {
  const kinds = [...new Set([...order, ...rows.map((r) => r.document)])];
  return kinds.map((kind) => ({ kind, series: rows.filter((r) => r.document === kind).sort((a, b) => a.series_name.localeCompare(b.series_name)) })).filter((g) => g.series.length > 0);
}

/** The series that is the default of its kind for the series' company (an empty company is every company's). */
export const defaultOf = (rows: Series[], s: Pick<Series, "document" | "company">): Series | undefined =>
  rows.find((r) => r.document === s.document && (r.company ?? null) === (s.company ?? null) && r.use_by_default && !r.disabled);

/** The writes that make `s` the default: the one that was loses it first, because the server keeps at most one per kind and company. */
export function makeDefaultPlan(rows: Series[], s: Series): { id: string; version: number; use_by_default: boolean }[] {
  const was = defaultOf(rows, s);
  return [...(was && was.id !== s.id ? [{ id: was.id, version: was.version, use_by_default: false }] : []), { id: s.id, version: s.version, use_by_default: true }];
}

/** What a document took: its series, or the usual number (no series named, or none given by the country's rules or the settings). */
export const took = (doc: { number_series?: string | null; number?: string | null }): { series: string | null; number: string | null } => ({ series: doc.number_series ?? null, number: doc.number ?? null });

/** The filter that finds the documents a series numbered. */
export const numberedBy = (id: string): string => JSON.stringify({ number_series: id });
