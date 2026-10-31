# Changelog

## The United Kingdom 0.1.0

- The UK's add-on, `ferrum-gb` (`addons/gb`, Ferrum localization.md L3), for a company in the UK:
  - VAT at the item's rate (standard 20%, reduced 5%, zero-rated, exempt); construction services
    to a VAT-registered customer who is not the end user reverse-charged (section 55A, the CIS
    reverse charge) with the invoice's mention; exported goods zero-rated; services to
    businesses abroad and digital services to consumers abroad outside the scope of UK VAT.
  - Purchases: services from abroad and CIS services reverse-charged by the company itself
    (input and owed VAT alike); imported goods carry no VAT on the supplier's invoice.
  - VAT registration numbers checked by HMRC's check digits.
  - VAT returns (UK VAT Return): the nine boxes worked out from the period's submitted invoices
    (boxes 6 and 7 in whole pounds, box 5 never negative), filed through HMRC's Making Tax
    Digital API (hmrc/vat-api's VAT (MTD) 1.0) once HMRC's declaration is made: Connect to HMRC
    signs in with OAuth (the callback at `/api/app/gb/hmrc/callback`), tokens are refreshed when
    they lapse, the fraud prevention headers (WEB_APP_VIA_SERVER) carry the filing browser's
    details when filed through `/api/app/gb/returns/{id}/file`, and HMRC's receipt or refusal is
    recorded on the return. A filed return cannot be cancelled.
- The European Union 0.1.2: TEDB's answers are read with quick-xml 0.42 (0.37 has two advisories,
  RUSTSEC-2026-0194 and RUSTSEC-2026-0195).

## Germany 0.1.0, and the European Union 0.1.1

- Germany's add-on, `ferrum-de` (`addons/de`, Ferrum localization.md L2), on the EU's:
  - XRechnung 3.0 (Download XRechnung): the EN 16931 invoice with Germany's CIUS, checked against
    KoSIT's rules (BR-DE-*, from its schematron) before it is written: payment instructions, the
    seller's contact, both parties' city and post code, a buyer reference (a public buyer's
    Leitweg-ID), a valid IBAN, and both electronic addresses (Peppol IDs, else the Leitweg-ID
    under scheme 0204 or an email under EM).
  - Small businesses (§ 19 UStG): no VAT on what they sell at home, with the invoice's mention;
    they invoice with their tax number.
  - The domestic reverse charge (§ 13b UStG): items marked so, sold to a business in Germany,
    carry no VAT and the mention that the customer owes it.
- The EU 0.1.1: a company's invoice address, contact and IBAN (checked by its check digits) go on
  its e-invoices (payment by SEPA credit transfer), and its tax number where it has no VAT number
  (EN 16931 BT-32), which also lets it invoice.

## 0.4.0, and the European Union 0.1.0

- The European Union's add-on, `ferrum-eu` (`addons/eu`, Ferrum localization.md L1). For a company
  in a member state it decides each sale's and purchase's VAT:
  - at home, the home country's rates;
  - goods to businesses elsewhere in the EU as intra-Community supplies (Article 138), services
    as reverse-charged (Article 196);
  - goods and digital services to consumers elsewhere in the EU at the destination's rates
    through the One-Stop Shop, when the company says so (above €10,000 a year);
  - exports exempt (Article 146), services to businesses outside the EU out of scope;
  - acquisitions from the EU and services from abroad reverse-charged by the company itself.

  Invoices carry the legal mentions, the customer's VAT number and VIES's consultation number. A
  supply without VAT to a business waits for the customer's VAT number to be confirmed in VIES,
  asked through the outbox whenever the number changes.
- Rates are data: the member states' standard rates come with the package (from the
  Commission's TEDB, each from the day it took effect), and Update EU VAT Rates fetches every
  rate from TEDB with the goods each reduced rate is for. A reduced-rate item carries its rate in
  each country, which Suggest EU Rates fills from its CN code; a rate is never guessed.
- EN 16931 e-invoices: Download e-Invoice (UBL) writes Peppol BIS Billing 3.0 (or plain EN 16931
  without Peppol IDs), checked against the standard's business rules first. Send via Peppol hands
  it to the company's access point connector through the outbox, which records delivery or
  failure. An invoice sent through Peppol is credited, not cancelled.
- Reports: the EC sales list, and the OSS return in euro.
- The localization seam gains `line_rates`: each line's own rates per tax account, set by a
  country after its item's (the EU's destination rates). Lines keep each tax account's category
  and exemption reason (`tax_categories`), which e-invoices read.

## India 0.2.1

- e-Invoices under reverse charge follow NIC's rules: an item's total leaves out the tax the buyer
  pays, as the invoice's total does (the IRP takes either, but the invoice's total must be its
  items'), and reverse charge is refused on exports (NIC allows it on B2B and SEZ invoices only).

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
