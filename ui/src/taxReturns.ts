// Tax returns (src/tax_return.rs, packages/accounting/entities/masters.yaml): a return is made from a
// definition (its boxes are data), a company and the period's first day; the server works the boxes
// out from the tax ledger each time the draft is saved, adjustments are typed with a reason, and
// submitting files it, which closes its period to invoices. Pure parts of the workbench.

import { add, cmp, isDecimal, isZero } from "./decimal";

export interface ReturnBox {
  id?: string;
  code: string;
  label?: string | null;
  computed: string;
  adjustment: string;
  adjustment_reason?: string | null;
  value: string;
}

export interface TaxReturn {
  id: string;
  version: number;
  docstatus: 0 | 1 | 2;
  number?: string | null;
  definition: string;
  company: string;
  from_date: string;
  to_date: string;
  filed_on?: string | null;
  boxes: ReturnBox[];
}

export interface DefinitionBox {
  code: string;
  label?: string | null;
  kind: "Sum" | "Formula" | "By Hand";
  measure?: "Tax" | "Base" | null;
  direction?: string | null;
}

export interface Definition {
  id: string;
  definition_name: string;
  frequency: string;
  boxes: DefinitionBox[];
}

export type Adjust = Record<string, { adjustment?: string; adjustment_reason?: string }>;

export const boxOf = (b: ReturnBox, a: Adjust): ReturnBox => ({ ...b, ...(a[b.code] ?? {}) } as ReturnBox);

/** The boxes that stand adjusted without a reason: the server refuses them, so the desk says so before. */
export function needReason(boxes: ReturnBox[], a: Adjust): string[] {
  return boxes.map((b) => boxOf(b, a)).filter((b) => isDecimal(b.adjustment || "0") && !isZero(b.adjustment || "0") && !(b.adjustment_reason ?? "").trim()).map((b) => b.code);
}

/** What the server is sent for the adjustments: the table whole, each box by its id with what a person may write. */
export const adjustBody = (boxes: ReturnBox[], a: Adjust) => ({
  boxes: boxes.map((b) => {
    const x = boxOf(b, a);
    return { id: x.id, adjustment: x.adjustment || "0", adjustment_reason: (x.adjustment_reason ?? "").trim() || null };
  }),
});

/** A box's value as filed: what the books give and what was adjusted by hand. */
export const filedValue = (b: ReturnBox): string => add(b.computed, b.adjustment || "0");

export const kindOf = (defs: DefinitionBox[] | undefined, code: string): DefinitionBox["kind"] | undefined => defs?.find((d) => d.code === code)?.kind;

/** A box whose value is a sum of documents can be drilled into; a formula or a box by hand cannot. */
export const drillable = (defs: DefinitionBox[] | undefined, code: string): boolean => kindOf(defs, code) === "Sum";

export const edited = (a: Adjust): boolean => Object.keys(a).length > 0;

export const status = (r: Pick<TaxReturn, "docstatus">): "draft" | "filed" | "cancelled" => (r.docstatus === 1 ? "filed" : r.docstatus === 2 ? "cancelled" : "draft");

/** The body that starts a return. */
export const startBody = (o: { definition: string; company: string; from: string }) => ({ definition: o.definition, company: o.company, from_date: o.from });

/** A return's drill-down: the answer of the report `tax_return_documents`. */
export interface Drill {
  rows: { voucher_type: string; voucher_id: string; voucher_no: string | null; posting_date: string; party: string | null; tax: string | null; rate: string; taxable: string; tax_amount: string; counted: string }[];
  totals?: { counted?: string } | null;
}

/** Whether the drill's documents add up to the box's computed value (a Sum box: always, but a stale view is worth saying). */
export const drillAgrees = (d: Drill, b: ReturnBox): boolean => cmp(d.totals?.counted ?? "0", b.computed) === 0;
