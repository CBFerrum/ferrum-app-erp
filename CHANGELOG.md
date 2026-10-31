# Changelog

## Malaysia 0.1.1, with fixes to the United States and the United Kingdom

- Malaysia's e-invoices through LHDN's MyInvois (`ferrum-countries`, `myinvois`): a submitted
  invoice or credit note of a company that uses MyInvois (Sandbox or Production) is written as a
  UBL 2.1 JSON document of version 1.0 with the supplier's TIN, BRN, SST number and MSIC code,
  both parties' addresses with their state codes, each line's classification (022 by default) and
  tax type (01 sales tax, 02 service tax, 06 not applicable, E exempt), and a credit note's
  reference to the invoice's UUID; the connector takes a token (client credentials), submits it
  (base64 and its SHA-256), asks after it until LHDN validates it, and records its UUID, long ID
  and validation link (the QR code's), or LHDN's errors; cancelling within the 72 hours after
  validation cancels it at LHDN, later a credit note corrects it; consumers buy under the general
  public's TIN (EI00000000010), buyers abroad under EI00000000020; what the company still needs to
  set is said before submitting.
- The United States and the United Kingdom: a company whose AvaTax or HMRC mode is Off never
  reaches the service, even when the test address override is set.

## Singapore, Australia, New Zealand, Japan and the UAE 0.1.1, and the European Union 0.1.6

- Peppol PINT e-invoices (`ferrum-countries`, `pint`): Singapore (PINT SG for InvoiceNow,
  `urn:peppol:pint:billing-1@sg-1`), Australia and New Zealand (PINT A-NZ, `@aunz-1`), Japan
  (JP PINT, `@jp-1`) and the UAE (PINT AE, `@ae-1`, transaction type 00000000), on the business
  process `urn:peppol:bis:billing`, with each country's tax scheme (GST; VAT in Japan and the UAE),
  its tax category codes (Singapore's SR, ZR, ESN33, OS; Japan's AA for the reduced rate; the
  UAE's Z for exports) and its participants' electronic addresses (UEN 0195, ABN 0151, NZBN 0088,
  the T-number 0221, the TIN 0235 from the TRN's first ten digits), checked against PINT's rules
  and sent through the company's Peppol access point on submit (to customers on Peppol) or by
  action; the access point's answer recorded; the companies' addresses and Peppol IDs.
- The EU's UBL writer takes a profile's business process, transaction type and tax scheme.

## France 0.1.0, and the European Union 0.1.5

- France (`fr`, in `ferrum-eu-members`): the e-invoicing reform (receiving for all and issuing
  for large and mid-sized businesses since 1 September 2026, the rest from 1 September 2027):
  invoices to French businesses queued on submit for the company's plateforme agréée as EN 16931
  UBL with both parties' SIREN (BT-30, BT-47, scheme 0002), the PA's lifecycle statuses recorded,
  an e-invoice corrected by an avoir rather than cancelled; the e-reporting report of the other
  sales (consumers, abroad) by day and rate; the invoice's mentions (the category of the
  operation, the option for VAT on debits, and between businesses the late-payment penalties and
  the €40 recovery indemnity); SIREN and SIRET checked (Luhn; La Poste's by its digits' sum), a
  French VAT number's key checked against its SIREN and the SIREN taken from it; the franchise en
  base (article 293 B) and the reverse charge on subcontracting in construction (283-2 nonies).
- The EU's UBL writes the parties' legal registration identifier (BT-30, BT-47) with its scheme.

## Spain 0.1.0

- Spain (`es`, in `ferrum-eu-members`): VERI*FACTU invoicing records (RD 1007/2023; systems
  adapted before 1 January 2027 for companies, 1 July 2027 for others, RDL 15/2025). Each
  submitted invoice gets its record (F1, F2 without the customer's NIF, R1 and R5 corrections),
  its SHA-256 fingerprint chained to the company's previous record exactly as AEAT's
  specification computes it (its three test vectors pass), and the QR code's address for AEAT's
  verification service (test or production, parameters URL-encoded) with "VERI*FACTU" below it;
  cancelling adds a cancellation record to the chain; with VERI*FACTU on, records (breakdown by
  S1, S2, N2, E1–E6) go to the company's AEAT connection through the outbox and AEAT's answer is
  recorded. NIFs checked (DNI, NIE, entities). The recargo de equivalencia (5.2%, 1.4%, 0.5%) on
  sales to retailers under it; the Canary Islands, Ceuta and Melilla outside the VAT area (goods
  exported, services to businesses there not subject).

## Romania 0.1.0, and the European Union 0.1.4

- Romania (`ro`, in `ferrum-eu-members`): every invoice to ANAF's e-Factura as CIUS-RO
  (`urn:efactura.mfinante.ro:CIUS-RO:1.0.1`): counties coded ISO 3166-2:RO (RO-CJ; RO-B for
  Bucharest, whose town is its sector, SECTOR1 to SECTOR6), checked against CIUS-RO's rules,
  queued for the company's ANAF connection on submit; ANAF's upload index, verdict and download
  ID recorded; a validated invoice is corrected by a storno, not cancelled; CIFs checked (key
  753217532); the small business exemption (article 310) and the domestic reverse charge
  (article 331).
- The EU's UBL writes the parties' country subdivision (BT-39, BT-54) from their addresses' state.

## Belgium, Italy and Poland 0.1.0, and the European Union 0.1.3

- `ferrum-eu-members` (`addons/eu-members`, Ferrum localization.md L2): member states on the EU's
  add-on, each its own app, with what they add to the EU's VAT at home (small businesses'
  exemptions and the domestic reverse charge, each with its note on the invoice):
  - Belgium (`be`): invoices to Belgian VAT-registered businesses go through Peppol on submit
    (mandatory since 1 January 2026; one without both parties' Peppol IDs is refused); enterprise
    numbers (KBO/BCE) checked (modulo 97) and turned into 0208 Peppol IDs; the small business
    franchise (article 56bis) and the contractors' reverse charge, in French and Dutch.
  - Italy (`it`): FatturaPA (FPR12, schema 1.2.3) from the EN 16931 model with the Italian data
    (codice fiscale, regime fiscale, REA, codice destinatario or PEC; XXXXXXX and CAP 00000
    abroad), the Natura of lines without VAT (exports N3.1, intra-Community N3.2, services abroad
    N2.1, exempt N4, the reverse charge N6.x), the €2 virtual stamp duty over €77.47 not subject to
    VAT, checked against SDI's rules and queued for the company's SDI channel with the next
    progressive; SDI's receipts recorded; a delivered invoice is corrected by a credit note, not
    cancelled; partite IVA checked; the regime forfettario (RF19, N2.2).
  - Poland (`pl`): FA(3) structured invoices for KSeF (namespace of 25 June 2025, variant 3) with
    the rate codes (23, 8, 5, 0 KR, 0 WDT, 0 EX, zw, np I, np II, oo) and amounts by rate, the
    annotations, payment by transfer; corrections (KOR) cite the corrected invoice's KSeF number;
    queued for the company's KSeF channel; KSeF numbers recorded after their CRC-8 check; NIPs
    checked; the small business exemption (article 113).
- The EU's Peppol hand-over is a function (`ubl::send_peppol`) that member states call on submit.

## South East Asia, the Gulf, Australia, New Zealand, Japan, Mexico and China 0.1.0

- `ferrum-countries` (`addons/countries`, Ferrum localization.md L6 and L7): thirteen apps, one per
  country, on one engine for VAT and GST that work the common way (each item's category takes its
  dated rate on one of the country's tax rows; exports zero-rated; purchases at home recover the
  tax where it is recoverable; imported goods carry none; services from abroad reverse-charged
  where the country says, for all its companies or those whose setting says so; an item's own tax
  rate wins; a sales invoice that charges the tax needs the company's registration number; the
  country's accounts on a new company), each with its registration number's check and its return:
  - Singapore (`sg`): GST 9% (8% in 2023, 7% before), RC Businesses' reverse charge, the GST F5.
  - Malaysia (`my`): sales tax 10% and 5%, service tax 8% (6% before March 2024) and 6%, each on
    its own row and not recovered, service tax on imported taxable services, the SST-02 figures.
  - Indonesia (`id`): PPN 12% on 11/12 of the price (11% in effect), luxury goods at 12%, the
    16-digit NPWP, SPT Masa PPN's figures.
  - Thailand (`th`): VAT 7% to 30 September 2027 (the Code's 10% after, unless extended), the
    13-digit tax ID's check digit, PP.30.
  - The Philippines (`ph`): VAT 12%, the 2550Q figures.
  - Vietnam (`vn`): VAT 10%, 5% and the temporary 8% (to 31 December 2026), declaration 01/GTGT.
  - Saudi Arabia (`sa`): VAT 15% (5% before July 2020), ZATCA's VAT return boxes.
  - The United Arab Emirates (`ae`): VAT 5%, the VAT 201 by the company's emirate.
  - Australia (`au`): GST 10%, GST-free and input-taxed supplies, the ABN's check, the BAS's GST
    labels.
  - New Zealand (`nz`): GST 15%, the IRD number's check, the GST101A on the 3/23 fraction.
  - Japan (`jp`): consumption tax 10% and the reduced 8% on rows of their own (worked out once per
    rate per invoice, as qualified invoices need), the T-number, the return's national and local
    parts.
  - Mexico (`mx`): IVA 16%, the border regions' 8%, 0% and exempt acts, the RFC, the IVA return on
    a cash basis from payments.
  - China (`cn`): VAT 13%, 9% and 6%, small-scale taxpayers' 1% (3% from 2028) without input
    credit, the unified social credit code's check character.

## The United States 0.1.0

- The United States' add-on, `ferrum-us` (`addons/us`, Ferrum localization.md L4), for a company in
  the United States:
  - Sales tax where the goods go (the ship-to, else the billing address; over the counter, the
    company's location), and within an origin-based state (Arizona, Illinois, Mississippi,
    Missouri, Ohio, Pennsylvania, Tennessee, Texas, Utah, Virginia) where they ship from.
  - The ZIP code's combined rate (state, county, city, special districts) from the states' rate
    tables: a US Rate Table takes a state's table as published (Avalara's free tables' columns,
    rates as fractions or percentages, every line checked) and writes its ZIP codes' rates (US
    Sales Tax Rate) from its date; cancelling it takes them back. Without a ZIP code's rate, the
    state's own (the Tax Foundation's, 1 July 2026), and where local taxes apply submitting
    refuses until the rate is there.
  - Only in the states where the company is registered to collect; none on sales through a
    marketplace facilitator; customers' exemption certificates (resale, government, charities...)
    by state and date; products' taxability by state (US Product Taxability: exempt or a reduced
    rate) and not-taxable tax codes (NT); exports.
  - Use tax on purchases the supplier did not tax, at the company's location's rate, accrued as a
    reverse charge (Use Tax Expense to Use Tax Payable).
  - Sales reported to Avalara AvaTax through the outbox: each submitted invoice recorded
    (`createoradjust`, committed) with the tax it charged as a tax-amount override, with its
    exemption certificate and entity use code; AvaTax's own figure comes back, and an invoice
    whose tax differs is marked for review; cancelling voids it there (`DocVoided`).
  - Reports: sales tax by state and jurisdiction; economic nexus (each state's threshold, from the
    Sales Tax Institute's chart of 1 August 2026, measured as the state does, against gross,
    retail or taxable sales and transactions); Forms 1099 (payments to suppliers in a calendar
    year, by bank rather than card, against $600 to 2025 and $2,000 from 2026; TINs sealed and
    shown by their last four digits).

## Canada 0.1.0

- Canada's add-on, `ferrum-ca` (`addons/ca`, Ferrum localization.md L5), for a company in Canada,
  with the CRA's rates as dated data: GST 5%; HST 13% (Ontario), 15% (New Brunswick,
  Newfoundland and Labrador, Prince Edward Island), 14% in Nova Scotia from 1 April 2025 (15%
  before); PST 7% (British Columbia), RST 7% (Manitoba), PST 6% (Saskatchewan); QST 9.975%.
  - Each line is taxed in its province of supply: goods where delivered (the ship-to), services by
    the customer's address; outside Canada, a zero-rated export.
  - PST or RST where the company collects it (its registrations), unless the item is not subject
    to it or the customer buys for resale; QST in Quebec when the company is registered.
  - Zero-rated and exempt items carry no GST/HST.
  - Purchases: the GST/HST (and QST in Quebec) the company recovers.
  - The GST/HST return (GST34 lines 101, 103, 106, 109) and the QST return, from the books.

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
