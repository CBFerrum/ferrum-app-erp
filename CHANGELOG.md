# Changelog

## 0.3.0

What every country's rules build on (Ferrum docs/plan/localization.md, L0); needs Ferrum b5b2d3f
or later.

- Tax categories and exemption reasons: tax rows and items' tax rates carry a category (Standard,
  Reduced, Zero Rated, Exempt, Reverse Charge, Intra-Community, Export, Out of Scope) and an
  exemption reason (a VATEX code or the law's words). Sales and purchase invoices keep their tax
  breakdown per tax, category and rate (EN 16931's BG-23): what is taxed and the tax, in the
  invoice's and the company's currency.
- Taxes per unit: the charge type On Item Quantity takes its rate as an amount per stock unit
  (excise, eco and packaging fees), and an item's tax rate can set its own amount.
- Reverse charge on a tax row: worked out and shown, never charged to the party. A purchase posts
  it to the row's account and to its reverse-charge account, so the supplier is owed the net; a
  sale posts nothing, and the portal shows it as the customer's to account for.
- Tax rounding per document (as before) or line by line, the company's choice.
- Orders, deliveries and invoices name the party's address and a ship-to (Address records, the
  party's primary ones unless chosen; one of another party gives way when the party changes).
  Countries' rules read where the goods go with `localization::ship_to`.
- India 0.2.0: a company's ISO country code decides whether India's rules apply (its name when it
  has no code). Purchases under reverse charge: a supplier or an invoice marked so takes the
  company's RCM templates, which new companies get with CGST, SGST and IGST RCM Payable accounts
  (a company set up before makes its own); services from abroad under reverse charge take IGST.
  e-Invoices mark reverse-charge sales (RegRev Y, the total without that tax) and report a cess
  per unit as non-ad-valorem cess, as e-way bills do.

Existing workspaces get the new fields and nothing else changes: documents keep their taxes and
totals until they are edited.

## 0.2.0

- Its packages and engine are this repository's (Ferrum docs/plan/erp-repo.md, stage 3): the
  packages accounting, inventory, buying, selling and manufacturing (`packages/`, byte for byte as
  workspaces have them, so nothing changes in a workspace and the version stays) and the engine on
  them (`src/`: ledger, open items, stock valuation, selling and buying, manufacturing and MRP,
  company set-up, the books' reports), which Point of Sale, Commerce, Assets, Field Service, HR
  and CRM build on. ferrum-domain keeps what every app shares.
- Countries plug in through `localization`: their rules for taxes (inside `pricing::calculate`, so
  ERP's documents and other apps' priced ones get them alike), checks before submitting and a new
  company's set-up. India is the first, now the add-on `ferrum-india` (`addons/india`) with GST,
  e-invoices and e-way bills. Its rules act only for companies in India (or naming no country): a
  company elsewhere in the same workspace gets no GST accounts, templates or checks. Every priced
  document gets the GST template for its place of supply, as invoices do: Commerce's carts and
  prices for a signed-in customer and Procurement's supplier quotations now too.

- ERP has a repository of its own (Ferrum docs/plan/erp-repo.md, stages 1 and 2): its manifest
  and India's, its interface (`ui/`, at `/erp/` and on the workspace's portal; built and committed
  in `ui/dist` and compiled into this crate), and its add-ons Finance, Procurement, Stock
  Operations and Manufacturing Operations (`addons/`, with their tests). The interface now draws
  the records frame from Ferrum-Frontend's packages (`@ferrum/records/frame`), as other apps'
  interfaces do. Its packages and their logic are still the platform's (stage 3). Earlier history
  is in the Ferrum and Ferrum-Frontend repositories.
