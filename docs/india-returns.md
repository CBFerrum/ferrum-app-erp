# India: withholding and GST returns as data

The India add-on seeds, when an administrator updates it, records an accountant edits; no rate, threshold, form or
period of India's is in ERP's code.

- **TDS categories** (Withholding Category): sections 194C (individual or HUF, others), 194H, 194J(a), 194J(b),
  194I(a), 194I(b) and 194Q, each with its dated rates and per-document and yearly thresholds, booked on the bill,
  rounded to whole rupees, to each company's TDS Payable (account number 2132 of the standard chart) unless the
  category names an account for the company. A party's lower-deduction certificate is a Withholding Certificate.
- **GSTR-3B and GSTR-1** (Tax Return Definition, country IN, monthly): boxes summed from the tax ledger by tax
  (CGST, SGST, IGST Output and Input), category, reverse charge and document kind; formulas for totals and the net
  payable. Exports: `india.gstr3b_json` (the summary in the shape of the portal's offline tool) and
  `india.gstr1_summary_csv`. Nothing is filed and no authority is called.

For exports and exempt sales to land in their boxes, their tax rows carry the category Export or Exempt (set it on
the export template's IGST row and on exempt items' tax rows).

## Boxes the return engine cannot express

They are By Hand boxes on the definitions; each needs what the tax ledger does not hold today.

| Return | Box | What the engine would need |
| --- | --- | --- |
| GSTR-3B | 3.1(a) cess | the cess as its own tax in the ledger (a `tax` filter would then do) — data, once a cess account exists |
| GSTR-3B | 3.2 inter-state supplies to unregistered persons, composition dealers and UIN holders, by state | the counterparty's registration status on each ledger row, and the place of supply at the level of a state (the `place` filter is a country) with a box per place |
| GSTR-3B | 4(A)(1), 4(A)(2) import of goods and services | a document kind for bills of entry and import of services, or a category per import on the tax row |
| GSTR-3B | 4(B) ITC reversed (rules 38, 42, 43, others) | reversal journals tagged with a reversal kind the ledger can filter on |
| GSTR-3B | 4(D) ineligible ITC | an "ineligible" mark on input tax rows |
| GSTR-3B | 3.1 and 4 against GSTR-2B | matching the purchase register against the statement downloaded from the portal: an import of that statement and a matching step; the engine sums only the company's own books |
| GSTR-1 | 4 B2B, 5 B2C large, 7 B2C others | the counterparty's registration status on each row, an invoice-value threshold per invoice (not per box), and the place of supply by state |
| GSTR-1 | 9A, 9C amendments | the original document of an amendment and its period |
| GSTR-1 | 12 HSN-wise summary | the item's classification code on each ledger row and a box per code (a grouping, not a sum) |
| GSTR-1 | 13 documents issued | counts of documents by series and cancelled ones: the engine sums amounts, not documents |

The withholding engine expresses the TDS sections; what it cannot: the monthly threshold of 194I from 1 April
2025 (a per-month aggregate), the higher rate without a PAN (section 206AA: the party's PAN status), and TCS under
206C (collected on sale: it needs an account the standard chart does not have; an accountant adds the category and
its account).
