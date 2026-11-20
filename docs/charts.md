# Charts of accounts

A company's accounts are made when the company is created: from the **standard chart** (`src/setup.rs`, `COA`), or
from a **Chart of Accounts Template** (Setup menu) chosen in the company's Chart of Accounts field. The chart is
chosen once: the field cannot be changed afterwards, and no existing account is renumbered, moved or deleted. A
template is a record, so an accountant can copy and adjust one, and a country add-on ships its country's as data.

## A template

Each row is an account: its number, name, parent (by number, within the template), whether it is a group, its root
type, account type and cash-flow section, and **Stands For**: the standard chart's number of the account whose part
it plays. A template is saved only whole (`setup::template_problems`):

- every number once; each parent a group of the same root type; no receivable or payable account as a group;
- no standard number twice;
- every role below mapped, to a leaf, and the two groups to groups.

| Number | Standard account | Role |
|---|---|---|
| 1111 | Debtors | default receivable |
| 2111 | Creditors | default payable |
| 4110 | Sales | default income |
| 5210 | Administrative Expenses | default expense |
| 2150 | Deferred Revenue | deferred revenue |
| 1170 | Prepaid Expenses | deferred expense |
| 1131 | Cash | default cash |
| 1121 | Primary Bank | default bank |
| 1141 | Stock In Hand | default inventory; the Stores and Finished Goods warehouses |
| 1142 | Work In Progress | the Work In Progress warehouse |
| 5110 | Cost of Goods Sold | cost of goods sold |
| 2121 | Stock Received But Not Billed | received not billed |
| 5120 | Stock Adjustment | stock adjustment |
| 5270 | Round Off | round off |
| 5260 | Exchange Gain or Loss | exchange gain or loss |
| 3200 | Retained Earnings | year-end closing |
| 3300 | Opening Balance Equity | opening balances |
| 5130 | Manufacturing Overheads Absorbed | overheads absorbed |
| 1151, 1152 | Input Tax, Withholding Tax Receivable | taxes the company claims |
| 2131, 2132 | Output Tax, Withholding Tax Payable | taxes the company owes |
| 1150, 2130 (groups) | Tax Assets, Duties and Taxes | where the add-ons put their own tax accounts |

## How code asks for an account

- `setup::standard_account(k, ctx, tx, company, number)`: on the standard chart, the account numbered `number`, as
  always; on a template's chart, the account standing for it. Never the one by the other's rule: a national chart
  may give "2131" to something else.
- `setup::ensure_account(k, ctx, tx, company, number, name, parent_number, root_type, account_type)`: that account,
  made when missing under the account standing for `parent_number` (numbered `number` on the standard chart,
  unnumbered on a template's; named `name (number)` when the name is taken). Every country add-on finds and makes its
  tax accounts this way, so a template company is taxed like any other, provided its template maps the parents the
  add-on needs.
- **Create Chart of Accounts** on a company (`accounting.company.setup_chart_of_accounts`) adds the accounts of its
  chart it lacks (its template's by number, or the standard ones by name) and sets default links still empty. Run
  again, it adds nothing.

No national chart ships yet: those come as add-ons' seed templates.

## Charts that ship

A country's add-on seeds its chart as a template, once per workspace (a later release does not rewrite a workspace's
copy). Each template's notes name the text it was built from, the day it was read, what was added to it, and that no
accountant has reviewed it. A chart lists the groups and the accounts down to three digits, plus the longer accounts the
roles need; a person copies the template and adds the rest.

| Template | Add-on | Built from |
|---|---|---|
| Spain: Plan General de Contabilidad | `es` | Real Decreto 1514/2007, Cuarta parte, in the Boletín Oficial del Estado |
| France: Plan comptable général | `fr` | ANC regulation 2014-03, article 1121-1, as consolidated on 1 January 2026 |
| Portugal: SNC | `pt` | Portaria 218/2015, Anexo 2, Base column, in the Diário da República, with Declaração de Retificação 41-A/2015 |
| Romania: Planul de conturi general | `ro` | OMFP 1802/2014, point 594, as the ANAF publishes it |

Countries that have no prescribed chart (the United Kingdom, the United States, India, Italy, Poland and the rest) use
the standard chart. Belgium, Germany (SKR 03 and SKR 04), Mexico and China ship none yet: see the erp-5 report.
A class that mixes assets and liabilities is not a row (a parent and its child share a root type): its subgroups are the
roots. Groupings and accounts the text does not have, which a role needed, are numbered `Z01`, `Z02` ... or named in
the notes.
