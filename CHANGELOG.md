# Changelog

## ERP 0.26.0: one basket pricing for the server and the till

A till must price a basket offline exactly as the server does, promotions included; Point of Sale carried its own replay
of ERP's pricing to do it, which its browser could not run. The money logic now exists once.

- **`ferrum-erp-pricing`** (`pricing/`, pure: no database, no clock, no floats, no country) prices a whole basket from
  data: a line's unit (`unit_factor`), its price list price by day and unit (`list_price`), its pricing rule
  (`best_rule`), its rates for the tax rows (`line_tax_rates`), what lines come to before promotions
  (`before_promotions`), the promotions engine with the free items it adds, `ferrum-calc` for taxes, discount and
  rounding, and the tax summary per rate (`tax_summary`). `price_basket` puts them together in the server's order.
  The server's `pricing::calculate`, `promotion::apply` and the given-promotions check now call these pieces instead
  of their own copies: the existing pricing, promotion, tax and rounding tests pass unchanged; ERP 0.25's tax
  breakdown is kept only inside a unit test that holds the summary to it on 300 generated documents, and Point of
  Sale 0.7.1's replay only inside a test that holds `price_basket` to it on 400 generated baskets (it differs in one
  place, where the server was right: a free item now takes its pricing rule as the server's lines do).
- **A versioned snapshot.** `pricing::snapshot` builds what a till keeps (`schema` 1, written down in
  `docs/pricing.md`): the days it prices for, currency and rounding, tax rows with their labels, items with units and
  their own rates, the price list's prices in force on some of the days, pricing rules without a condition (a
  condition stays the server's), the promotions without a code or a first order, the walk-in customer, channel and
  place. A test prices five baskets through the server's draft and through the crate with a snapshot of the same
  records: line amounts, totals and promotions agree.
- **In the browser.** `priceBasket` (JSON in, JSON out). The workspace pins wasm-bindgen 0.2.129, the version
  Ferrum-Frontend's `@ferrum/calc` is built with, so one `wasm-bindgen-cli` builds both. `scripts/pricing-wasm.sh`
  builds it (local paths remapped) and prints the files' SHA-256 digests; `--check` prices 60 fixture baskets through
  the WebAssembly in Node and compares the answers byte for byte with what the native crate gives (the fixtures and
  their answers are kept in `pricing/tests/`, held by a native test).
- **Promotions an app applied, as a share.** With `given_promotions`, a line's `promotion_percent` as the app priced
  it is kept (the line carries the discount as ERP's own pricing would) instead of being cleared; lowering the price
  as before still works. The check against the promotion records is unchanged.
- **The Promotions Given report** reads every entity with promotions applied and a company, date, channel and
  conversion rate, not only sales orders and invoices: a till's sales are counted.

Not done: `price_lines` (the contracted query) still prices through the server's draft, not the crate; the browser
package `@ferrum/pricing` in Ferrum-Frontend (the files this script builds, committed beside `@ferrum/calc`). Seen and
left: a bundle that names one item in two parts can take more than that item's line (the engine counts the line twice).
## ERP 0.25.1: the screens 0.19.0 to 0.25.0 asked for

Eight desks in ERP's own interface, for server features that were only generic forms until now. Each loads when first opened, shows only where its package is installed and only to people who can read what it works on, and names no country, tax, form or currency: labels and choices are the server's (the localization seam, return definitions, withholding categories, templates). The server is unchanged.

- **Payment run desk** (`/e/PaymentRun/new`, `/desk/payment-run/:id`): filters, propose, review and edit the bills (discount, amount, instrument number, left out), the approval state where a policy is on, submit, make the payments once, the bank file, the remittance messages sent for each payment, each failed line with the server's reason and a retry that pays only what is not paid.
- **Stored value desk** (`/desk/stored-value`, `/e/StoredValueAccount/new`): find a card (balance, held, available, state, expiry, movements), issue, top up, hold for an order, release. A code and PIN are password boxes sent only to the kind's `prove` and `balance`, cleared as the call starts, and never in the address, a cache key, a toast or storage; a code the server makes is shown once.
- **Promotions desk** (`/desk/promotions/:id?`): a promotion's fields grouped (what it is, schedule, conditions, what it gives, limits, stacking) and changed where they stand; a test basket priced by the server's `price_lines`, saving nothing, with what the chosen promotion took; the Promotions Given report by promotion.
- **Tax return workbench** (`/e/TaxReturn/new`, `/desk/tax-return/:id`): a return definition and a period, boxes with how each is worked out, drill-down to the tax ledger documents behind a sum box, adjustments with reasons, the files the templates give, filing (which closes the period to invoices) and cancelling.
- **Withholding desk** (`/desk/withholding`): categories with dated rates and thresholds, the rate in force marked; a statement per party and period as a file; what is payable by category and the account it books to.
- **Shipping desk** (`/desk/shipping`, `/desk/shipping/:id`): parcels, rates, choosing one, buying the label once (disabled while sent, with the request's key), the label file, tracking, cancelling; carrier connections with the state of the platform connection each goes through and a link to Administration.
- **Planning desk** (`/desk/planning`): planned orders by period per item and warehouse, taken in bulk (what is left out is set to nothing) and made into orders; action messages; the promise-date check on an order's lines; reservations per line under the company's policy.
- **Numbering desk** (`/desk/numbering`): series per kind with template, gapless and default flags, the last number each gave, making one the default, and which series a document took.

Reaching them: a record that has a desk (payment run, tax return, shipment, promotion, MRP run) opens on it at its usual address, with the record's own form, actions and all, a tab away; the others answer the list page of their menu row (stored value accounts, withholding categories, numbering series, promotions, shipments, MRP runs), the plain list at `/l/<Entity>`. The Create menu offers payment run, tax return, stored value card and shipment. Menu rows are the existing ones, shown only to people who can read the entity.

Not shown, for lack of a route: the number a series gives next (the platform's counter has none; the last number given is shown), and the list of a stored value account's holds (only the hold a call returns is known; others are released by number). Words of the new screens are in English until translated.

## ERP 0.25.0: what other apps had to copy, numbering series, carriers through connections, secret proofs, approvals that ship

Follow-ups to 0.24.1 that other apps and the platform's connections asked for.

- **Promotions an app already applied.** `promotion::to_engine(&Doc, rate)` is public with a stable signature, with `engine_line` (a document line as the computation takes it) and `iso_weekday`, and `ferrum_erp_promotions::admits` (whether a promotion is in force at a moment): what Point of Sale had copied into its snapshot. A sale an app priced itself (a till) is passed on the invoice or order as `given_promotions` (`{snapshot, source, counted, at, applied: [{promotion, line, amount, code}]}`, the lines already net of it). Each promotion must exist, be the company's (or every company's), have been in force at the sale's moment (dates, weekdays, hours, customer, channel, place, a good code; switching it off since does not void the sale), and no amount may exceed what the computation gives for that promotion on that line, over the lines as they were before it and with only the promotions given, plus one smallest currency unit for its rounding. What is given becomes the document's **Promotions Applied**, so the **Promotions Given** report counts it, a credit note gives back its share, and a usage limit counts it: once per submission by ERP's conditional update, or not at all when the app says `counted` (it counted already). A `source` (the app's id for the sale) is recorded on one submitted document only, however many are submitted at once (an advisory lock on the source, then a look at the submitted documents).
- **A numbering series chosen per document.** **Numbering Series** (Setup) defines, for a kind of document (sales and purchase orders, deliveries, receipts and invoices, stock and journal entries, payments), a template as usual (`TINV-{FY}-{seq:5}`), whether it is gapless, whether it is the default of its kind (per company). A document's **Numbering Series** field, set by a person, an app or a rule, names the one it takes when it is submitted; else the localization seam may answer a series by name (`Localization::number_series`); else the default of its kind; else the usual number, as before. A series of another kind, another company's or a switched-off one is refused at creation. A gapless series takes its counter inside the submission, so documents refused after taking a number leave no gap.
- **Carriers through connections.** Saving a **Carrier Connection** makes one platform connection for it (one more for each further address its calls go to), whose operations `rate`, `label`, `track` and `cancel` are its calls; ERP's own HTTP client is gone (the label's file, when the answer gives an address, is still fetched with the platform's vetted client). Descriptions written for 0.24 work as they were. The API Key, API Secret and Inbound Secret are moved into the connection when typed and cleared from the carrier; the upkeep puts each carrier made before on a connection once (turned on only if the carrier was). A label is bought once under a double press and under a rolled-back run (the connection's call key). Tracking calls go to the connection's inbound address; `auth: token` is checked by the platform; `hmac` (a body signed without a timestamp) is not a scheme it checks, so such a carrier is tracked by the upkeep (6 hours when it had none). The old address under `/api/site/{workspace}/inventory/carriers/` is gone: point carriers at `/api/connections/inbound/{id}`.
- **Stored value's proof as a secret parameter.** `prove` and `balance` take the card's code and PIN as `secret: true` parameters: the platform keeps them out of the audit trail, approval requests and error texts. Nothing of ERP's own masking duplicated that.
- **Approvals that ship.** Purchase orders (by **Purchase Approval Settings**' bands) and payments out (new **Payment Approval Settings**: a payment to a supplier or a refund to a customer) have default approval policies in their packages' `approvals:`, off until **Ask for Approval** is ticked. The person who made the document never approves it; stand-ins and escalation are the platform's. 0.23's `docs/approvals/purchase_order.json` is gone (a workspace that applied it keeps its own policy).

For data already there: nothing changes until an administrator updates the app and then makes a series, passes promotions, ticks Ask for Approval; carriers are moved by the upkeep on the first start. Screens: Carrier Connection's form shows **Connection**; Numbering Series, Payment Approval Settings and the new fields are generic forms. `ui/` is not rebuilt.

## ERP 0.24.1, Finance 0.6.1: two faults of workspaces not yet updated

Both were in code released in 0.23.0 and 0.24.0 and neither showed in a test, because every test started from a fresh install.

- **Carrier tracking logged a database error on every start, for every workspace.** The upkeep asked Postgres for `make_interval(hours => c.track_every_hours)`, and the column is a bigint where `make_interval` takes an integer. Shipments were never tracked by the upkeep. The hours are now cast. The same mistake was searched for in every crate (`make_interval`, interval arithmetic on bound values, dates against mismatched types); the other `make_interval` already cast.
- **Finance's backfill of the payment run lines' supplier email failed and was queued again, for ever.** It named the child entity `PaymentRunLine`, which has no table of its own, so the platform built SQL with an empty table name. It now names the lines' table, `e_payment_run__lines`. Workspaces with Finance 0.5 or older fill the email the next time backfills run, with or without the update applied (the schema is synced when the server starts; only seeds and versions wait for the administrator). No other backfill or upkeep of ERP or its add-ons assumes a table, column or seeded row the workspace may not have.
- **An upgrade test** (`addons/finance/tests/upgrade_it.rs`) installs ERP 0.22.0 and Finance 0.5.0 from their package files at commit `51cefe5`, makes documents, boots this build on the same database, runs every upkeep and backfill before and after the update, and asserts that nothing is logged at warning level, no job fails and the documents are intact. CI now fetches the whole history for it. The carrier test now runs the upkeep's tracking.

No administrator action; nothing in `ui/` changed.

## ERP 0.24.0, India 0.6.0, Stock Operations 0.2.0, Manufacturing Operations 0.2.0, Warehouse Management 0.3.0: carriers, labels and scanning, goods in transit, rejected goods, by-products, planning by period, India's TDS and GST returns as data

Operations rows OP-17, OP-20, OP-24, OP-28, OP-31, OP-39 and OP-42, and finance rows FN-28 and FN-38 in part.

- **Carriers behind one interface (OP-17).** A **Carrier Connection** (Inventory) describes four HTTP calls — rates, label, tracking, cancel — as templates over the shipment and JSON pointers into the answers (rates, the label file in base64 or at a URL, the tracking number and link, the cost, the error, which statuses mean later); its API Key, API Secret and Inbound Secret are secret fields; a Test Carrier answers from fixtures. Ship by Carrier on a delivery note or a purchase return makes a **Carrier Shipment** with its parcels (weight, dimensions): Get Rates, Choose Rate, Buy Label (the file attached, tracking number and link kept; the row is locked first and the same idempotency key goes with every attempt, so it is bought once), Track (asked, by the upkeep every *Track Every (Hours)*, or by the carrier's own call to `/api/site/{workspace}/inventory/carriers/{connection}`, signed by HMAC or a token, refusals counted), Cancel Label. *Label Cost*: Not Booked (the default), Absorbed (freight booked against Freight Owed, taken back on cancel), or Charged to Customer (also a line of the Freight Item on the invoice made from the delivery, once). No carrier is named in the code; `docs/carriers.md` has two worked examples.
- **Labels (OP-20).** Document templates `inventory.item_label` (a small page with a barcode), `inventory.item_label_printer` (label-printer text), `inventory.parcel_labels`, `buying.serial_labels`, `stock_ops.batch_label`, `wms.bin_label`, printed for many records at once by the platform's labels call.
- **Scanning (OP-31, the server's part).** The company's read-only capability `core.company.scan` says what a scanned text is: an item by barcode or code, a batch, a serial, a bin, a document by number or a shipment by tracking number, a GS1 element string's item (by its GTIN), batch, expiry, serial and quantity; with the unit, at most 20 matches, ambiguous when more than one answers.
- **Goods in transit (OP-28).** A Material Transfer may *Ship Now, Receive Later*: the goods go into a transit warehouse (the entry's, the company's new **Transit Warehouse**, else its first, made when it has none) at their value; **Receive** makes the receipt, in parts, with what is short or damaged written off (stock adjustment or an account given) or claimed (to an account) with a reason; the transfer's lock stops two receipts taking the same goods; cancelling gives them back. Warehouses have **Holds** (Stock, Transit, Rejected): only Stock is offered to orders, deliveries and promising. Report **Goods in Transit**; Stock Balance shows what each warehouse holds.
- **Rejected goods (OP-24).** A receipt's **Rejected Qty** goes into its line's or the company's new **Rejected Warehouse** (made when there is none) at no value, never billed, the order counting only what was accepted. **Return Rejected Goods** makes the return of what is left (never more than was rejected, the order untouched) or a debit note against the invoice that billed the receipt. A quality inspection may be Accepted with a **Rejected Qty**: that part moves to rejected when the receipt is submitted and the receipt is priced again.
- **By-products and co-products (OP-42).** A BOM's **By-products and Co-products**: by-products valued first at a value per unit (scrap with a recovery value) or a share of cost; co-products share the rest with the main item by quantity, a ratio or standard values (**Co-products Share Cost By**, **Main Item's Ratio or Standard Value**). Production and subcontracting receipts receive them; the values sum to the cent.
- **Planning by period (OP-39).** An MRP run may **Plan By** day, week or month over a **Horizon** and **Plan Across** several warehouses: demand (sales orders by delivery day, work orders' materials, planned parents), stock and orders already there by period; a warehouse short takes another's spare stock by a proposed transfer before anything is bought; new orders lot-sized by the item's **Planning by Warehouse** (safety stock, minimum, multiple, maximum, lead time); **Action Messages** on existing orders: bring forward, push out, cancel, increase, decrease. Create Orders also makes the transfer. Both planners now net and lot-size with the same code (`planning::net`, `planning::params`): the run without a period and the production plan's raw materials.
- **India's withholding and GST returns as data (FN-28, FN-38 in part).** India seeds TDS categories (194C, 194H, 194J(a), 194J(b), 194I(a), 194I(b), 194Q) with dated rates and thresholds, booked to each company's TDS Payable by its account number (a category's new **Account Number**, for companies without a row of their own), and GSTR-3B and GSTR-1 as tax return definitions with their exports `india.gstr3b_json` and `india.gstr1_summary_csv`. Boxes the engine cannot express are By Hand and listed in `docs/india-returns.md`. Nothing is filed.

For data already there: nothing changes until a connection, a transit transfer, a rejected quantity, a BOM's other outputs, a run by period or a planning row is made; seeded categories and returns arrive when an administrator updates India. Rejected quantities on receipts submitted from now are kept in stock.

## ERP 0.23.0, Finance 0.6.0: promotions on changed orders, stored value held for orders, erasure, dimensions everywhere, promising and holding stock, purchase approvals

Loose ends other apps reported against 0.19.0 to 0.22.0, and the first operations rows (OP-10, OP-11, OP-21).

- **Promotions when an order changes.** Update Items on a submitted order prices its promotions again: a code it already counted stays good though used up since, a promotion the change brings in is counted once, one it drops is given back. A credit note gives back each returned line's promotions (`applied_promotions`, negative), so the Promotions Given report nets returns; a line returned in parts takes its share in proportion and the part that returns the rest takes exactly what is left (`promotion_returned` on the line keeps what was given back), so the parts sum to the cent. A return counts no code.
- **Stored value before the invoice.** A kind taxed when redeemed is untaxed on orders and quotations as on invoices. **Holds**: `accounting.stored_value_kind.hold` (with a proof, `prove` takes `hold`) sets an amount of an account aside for a submitted sales order; the account shows it as `held`, no redemption or transfer may take it, `balance` answers `held` and `available`; it ends once: released (`release`, or the order cancelled), expired after the kind's new **Hours an Order Holds Value** (72), or redeemed as one payment from the account when an invoice of the order is submitted. Public `stored_value::hold` and `release`.
- **Instrument numbers.** A mode of payment's company row may carry a book of numbers (next, last, prefix, digits): a payment out by instrument without a number takes the next on submission (one locked update, never the same number twice; a used-up book is refused), and is dated its posting date when undated. Finance's payment run lines take a cheque number each.
- **Erasure reaches 0.19.0 to 0.22.0.** A supplier's IBAN and BIC (Finance) are erased with their email; payment run lines keep the supplier's email and lose name, IBAN and BIC with them (a backfill copies the email onto earlier lines). Retained, and named in the answer: payments, payment instruments (which now link their customer or supplier, backfilled), withholding certificates and bad-debt write-offs, as the books; the stored value account as before. The bank file a run kept stays with the run.
- **Dimensions everywhere.** The trial balance (movement per value), general ledger (lines by value, each subtotalled) and cash flow take `group_by` as profit and loss and the balance sheet do. The tax an early-payment discount takes back is kept on the payment (`discount_taxes`) and reaches the tax summary and tax returns on the payment's day, as less output or input tax.
- **What can be promised (OP-10).** A draft order's stock line shows what is available at its warehouse (`available_qty`) and gets a **Promised Date** when it has none: today when it is there, the day a purchase order or work order brings enough, else the item's lead time. The company's read-only capability `core.company.availability` answers the same per warehouse without saving.
- **Holding stock (OP-11).** Company settings **Stock Reservation** (Count Only, as before; or Hold), **Reserve When** (Submit or On Demand) and **Reservation Lasts (Days)**, and an item group's own **Stock Reservation**. Under Hold an order's line holds what is free when it is submitted (and changed, or when Reserve Stock is pressed), the rest back-ordered (`stock_reserved_qty`, `backorder_qty`, `reserved_until`); the stock balance is locked first, so of two orders racing for the last stock one holds it; no delivery, sale or stock entry may take what other orders hold, and the order's own delivery uses its reservation. Release Reservation, cancelling, closing and the time limit give it back.
- **Purchase approvals (OP-21).** **Purchase Approval Settings** keeps value bands; `docs/approvals/purchase_order.json` is the approval policy on submitting a purchase order that reads them, applied once as the README says (a package cannot ship one).

For data already there: nothing changes until a company chooses Hold, a mode gets a book of numbers, a kind's order is held, or the approval is applied. Promised dates fill on drafts saved from now.

## ERP 0.22.0, Finance 0.5.0, EU 0.3.0: dimensions, instruments, payment runs, write-offs, recurring journals and tax returns

- **Accounting dimensions** (`AccountingDimension`): a code, its values (a list, or the records of an entity), the accounts, account types or root types that need it, and defaults by company, cost centre, project, party and item (first matching row). Journal lines, payments and their deductions, sales and purchase invoices and their lines carry `dimensions` ({code: value}, a line's over its document's); every posting fills defaults, refuses an unknown value or a required one missing, and writes them to the ledger's `dims`, served by the partial GIN index `journal_line_dims`. Lines from before read as none; reversals mirror their originals. The trial balance, general ledger, profit and loss, balance sheet and cash flow take `dimension` and `dimension_value` (cost_center and project too); profit and loss and the balance sheet take `group_by` (a column per value, one for none, and their total). Finance's budgets may be against a dimension's value (what is booked; orders carry no dimensions).
- **Payment instruments**: a Mode of Payment may be paid by instrument, with its instruments received and issued accounts per company and a charge for a bounce. A payment by instrument records the number, the date written on it and its bank, books to the instruments account and makes a Payment Instrument; Deposit, Cleared (not before its date, into the bank), Bounced (reverses a clearing, takes the payment back so its invoices are owed again, charges the customer unless left off, raises `accounting.instrument_bounced`), Cancel Instrument and Replace. The upkeep sends `accounting.instrument_due` once when a received instrument's date comes.
- **Payment runs** (Finance, `PaymentRun`): propose bills due by a day and those whose early-payment discount is open on the pay date, leaving out bills on hold (Hold Payment / Release Payment on a bill), bills another run holds and suppliers under the least; adjust; submit (the workspace's approval policy on Payment Run, if it sets one); Make Payments: one payment per supplier as the person pressing it, each in a savepoint (refused ones Failed with why), remittance advice `finance.remittance_advice`, and the bank's file from the run's template (`finance.bank_transfer_pain001`, ISO 20022 pain.001.001.03, or `finance.bank_transfer_csv`; a bank's own is a template) kept on the run. Pressed again it pays nothing twice. Suppliers have an IBAN / account and a BIC / bank code.
- **Early-payment discounts**: a payment's allocation may take a discount to an account, the tax taken back in the invoice's proportion when it reduces the tax; payment terms say the accounts per company and the treatment, and Finance takes the discount by itself on a payment in time.
- **Bad-debt write-off** (`BadDebtWriteOff`, `WriteOffReason`): one action, to the reason's account, less the relief a country gives through `Localization::bad_debt_relief` (none by default); Record Recovery books what comes back, the relief's share owed again.
- **Recurring journals** (`JournalTemplate`): lines made into an entry every month, quarter, half-year or year until an end date or a count, submitted or as drafts, reversed the day after for accruals; once per period under a lock and a unique (template, period).
- **Tax returns** (`TaxReturnDefinition`, `TaxReturn`): boxes summed from the tax ledger by tax, rate, category, direction, place and document kind, formulas in exact decimals, boxes by hand; adjustments with reasons; filing keeps the values and closes the period to invoices; the report Tax Return Box drills to documents; `accounting.tax_return_csv` exports it. The EU add-on seeds Ireland's VAT3.

For data already there: nothing changes until someone defines a dimension, a mode of payment by instrument, a run, a reason, a template or a return definition.

## ERP 0.21.0, India 0.5.0, EU 0.2.0, UK 0.2.0, thirteen countries 0.2.0: promotions, and tax registrations through the seam

Promotions over a whole sale, for orders, invoices, the till and the web shop alike (POS PS-16, PS-40, PS-66, PS-69 to PS-71; Commerce CM-60 to CM-62; operations OP-7).

- **A promotion** (`Promotion`, Selling menu) says when it applies (dates, weekdays, hours in the company's new Time Zone, customers or customer groups, channels a document names, warehouses or places an app names, a code, a first order, a minimum spend or quantity over a set of items), what it gives (percent or amount off the document or the qualifying lines, buy some and get some free or at a percent or a price, cheapest or dearest first, a bundle price, free delivery, a free item added as a line) and how it combines (priority, alone or with others, also on lines with their own discount or not, a cap).
- **Pure and shared.** `ferrum-erp-promotions` computes it without a clock or a database (feature `wasm` for the browser, beside `ferrum-calc`), so a till prices offline exactly as the server. Item-level pricing rules come first; promotions then go by priority on what each line has left. A discount on several lines is spread by largest remainder; each line keeps its exact share (`promotion_percent`), so its taxes are on what is left and a return of part of it gives back the same share.
- **Kept on the document.** `applied_promotions` says what each promotion took off each line, with its code. Promotions run on a draft order or invoice of its own; an invoice made from an order, a delivery, a return or a debit note keeps the shares it copied.
- **Codes** (`PromotionCode`): one shared code or many unique ones, each with its own limit and the promotion's overall one; counted once when a document is submitted, given back when it is cancelled; of two documents taking the last use at once, one gets it. An unknown code and a used-up one get the same answer.
- **Delivery charges** are lines of an item marked Delivery Charge: only free delivery takes them off.
- **Report** Promotions Given: discount by promotion, month and channel, in company currency.
- **For other apps:** `promotion::price_lines` and the read-only contracted query `core.company.price_lines` (lines, customer, channel, place, moment, codes, currency, price list in; priced lines, promotions applied, codes rejected and totals out; nothing saved, no code counted); `promotion::candidates` gives a till the promotions to take offline; `promotion::count_uses` counts a document's codes.

A country's tax registrations, through the localization seam: `Localization::tax_ids` (a company's or a party's registrations, each with its label and value) and `Localization::registered` (a registered business or not), asked with `localization::tax_ids` and `localization::registered_business`; the core says nothing by itself. India answers with the GSTIN and its GST category, the EU with the VAT number, the UK with the VAT registration number and each of the thirteen countries with its registration number. A draft sales order or invoice keeps the seller's and buyer's in `tax_registrations`, which their prints read; documents priced before keep printing as they did.

## ERP 0.20.1: a card's code and PIN never come to rest

The platform writes an action's parameters to the audit log and a capability's result to its receipt, so 0.20.0 logged the code, PIN and other card's code given to stored value's issue, redeem, refund and transfer and an account's New Code PIN, and kept codes made for new cards in the receipts.

- **Prove, then act.** A code and PIN go only to the kind's new `prove` (`accounting.stored_value_kind.prove`, read only, never logged), which keeps their hashes and answers a proof for one operation on one kind, used once, gone after five minutes; a code it makes is answered there, once. `issue`, `redeem`, `refund` and `transfer` take the proof (`transfer` its other card's as `to_proof`) and refuse a code or PIN; their results no longer carry a code. Under the same effect key the same code gets the same proof, so retries still repeat. New Code takes a proof, or makes the code and, for a kind that needs one, a PIN, shown once. The public functions in `stored_value` keep their signatures and prove inside. **Callers of the capabilities change**: prove first, then pass the proof.
- **Cleared.** On install, the codes and PINs 0.20.0 wrote are blanked in those operations' own audit events (the redaction recorded, the chain unbroken) and the made codes taken out of their receipts; nothing else is touched.

## ERP 0.20.0: stored value

Gift cards, store credit and prepaid packages, built once beside the ledger so the till, the web shop and Bookings use the same thing (POS PS-34 to PS-37, Commerce CM-42, Bookings BK-27 and BK-29).

- **Kinds.** A Stored Value Kind holds money in the company's currency or a count of units (sessions, washes) that pay for the items it lists; is held by a bearer or by the customer who buys it; sits in a liability account; and says whether it can be topped up, transferred, cashed out or used in part, whether refunds go back onto it, whether it needs a PIN, who may issue it by hand, its least and most loads and balance, its expiry (none, or so many days from issue or from last use) with a warning period, what unused value becomes (stays owed, income at expiry, or income over time at a stated expected share), and whether tax is charged when it is sold or when it is redeemed. Nothing is seeded.
- **Accounts and their ledger.** A Stored Value Account keeps its balance and the value the books owe, each the sum of its own append-only ledger, every entry tied to its document and journal entry, never below nothing in the database; changes lock the account first, so of two tills taking from one card one waits and is then told what is left. Codes are kept as a salted hash and their last four characters; wrong PINs and unknown codes are limited. Block, unblock, a new code (shown once), adjust and cash out.
- **Selling.** An item names the kind it sells (and the units one loads); its invoice lines go to the liability, taxed as any line or not at all as the kind says, and open or top up an account on submit. Cancelling the sale takes the value back, refused once it has been used. Issue by hand against the kind's Adjustment Account.
- **Redeeming.** A payment entry naming an account or a code pays a customer's invoices from it, the liability in the bank account's place; units pay their items' lines at what they were bought for. Refund on a credit note goes back onto the card that paid, or onto new store credit for a return without its receipt.
- **Expiry** by ERP's upkeep, once per account, with the event `accounting.stored_value_expiring` raised once before it.
- **Reports**: Stored Value Outstanding (by kind and age), Stored Value Statement, Stored Value Issued and Used.
- **Capabilities** `accounting.stored_value_kind.balance`, `issue`, `redeem`, `refund`, `transfer` and `reverse`, once per effect key, each undone by `reverse`, and the same as public functions in `stored_value`.

## ERP 0.19.1, Warehouse 0.2.3, Shop Floor 0.2.3: the tuned design system

Rebuilt with Ferrum-Frontend 45dad7f, the design system as tuned after the landed build was reviewed. The left menu's icons are chosen so that none repeats on more than three rows and each fits its row; their drawings load beside the first page instead of inside it, so the page starts lighter; a row that scrolls sideways fades at the edge where there is more, in every browser and with reduced motion; a printed record leaves out empty fields and keeps its totals with the table; menus and popovers sit inside the page's landmarks for assistive technology; a selection bar folds what does not fit into More on a narrow screen. Nothing else changes.

## ERP 0.19.0, India 0.4.0, Canada 0.1.1: delivery region, tax summary and withholding

- **Where the goods go, without an address.** Sales orders, deliveries and invoices, and purchase
  orders, receipts and invoices, have a delivery country and region. Countries' rules read one
  thing, `localization::ship_to`: the ship-to address, else those two fields, else the party's
  address. A draft's delivery fields follow its ship-to address, and a typed country becomes its
  ISO code, on any document priced through `calculate`. Canada reads it; India's place of supply on
  a sale is the state the goods are delivered to when the document states a delivery, then the
  place typed on the invoice, then the customer's registration as before.
- **Tax summary.** A report for any period of the tax each submitted sales and purchase invoice
  stored in its tax breakdown, by tax account, category, rate and reverse charge, output and
  input: the taxable base and the tax in company currency, credit notes netting, cancelled
  invoices left out, each group opening on its invoices. In the Accounting menu.
- **Withholding by category.** Withholding categories (deducted on the bill, deducted on payment,
  or collected on a sale; on the net or the grand total; dated rates with per-document and yearly
  thresholds; the whole year's base or only the part above the threshold once it is crossed;
  rounding to whole units; an account per company) and withholding certificates (a lower rate or
  none, for a period, up to a limit). Suppliers, customers and items name a category and an
  invoice or payment can name its own. Bills deduct, payments deduct in proportion to what they
  settle and on what they advance (a later bill is withheld only on the rest), sales invoices
  collect; returns take back their share and cancellations reverse. A Withholding report lists
  every document's rows. No category is seeded: a country's are its add-on's to add.

## ERP 0.18.2, Finance 0.4.2, Stock Operations 0.1.2, Manufacturing Operations 0.1.2: item group accounts and batch expiry

- **Item groups' accounts are read.** A line that names no income or expense account takes the
  item's, then its item group's or the nearest parent group's, then the company default. Invoice
  and return lines, the posting of a line that still has none, Finance's purchase order budget
  dimensions and material request commitments, and the subcontracting receipt's service charge all
  pick it the same way. A stock item bought still waits on stock received but not billed.
- **Expired and disabled batches no longer leave for use.** A delivery, a sale that updates stock,
  a product bundle's delivery or a production consumption is refused a disabled batch or one past
  its expiry on the posting date, naming the batch and its expiry. Writing it off (a material
  issue), moving it to another warehouse, sending it back to the supplier, counting or revaluing it,
  cancelling an earlier posting and a customer's return still work.

## ERP 0.18.1, Warehouse 0.2.2, Shop Floor 0.2.2: the redesigned interface

The interface follows the upgraded design system, rebuilt with Ferrum-Frontend 0a47157. Text uses the kit's type roles (nothing under 12px) and controls its height tokens; loading shows skeletons shaped like what is coming; empty, no matches, denied, offline and failed are told apart; status colours come from stored state ids. The interface check now runs the UI lint against an empty baseline. ERP's own pages are recomposed: the home strip is linked counts by urgency, Accounts today is a prioritised queue, and statements name their company, period and currency and mark groups and totals with weight and rules; a negative no longer flips its sign right to left. Warehouse and Shop Floor are rebuilt on the kit's frame, tabs, page heads and states (they had been built before the redesign); scan and confirm buttons are 44px on touch; a failed queue says so.

## ERP 0.18.0, Finance 0.4.1, Procurement 0.1.4: orders that change, and credit that holds

- **Update Items** on a submitted sales or purchase order (a person who may submit it): a line's
  quantity up or down, never below what was delivered, received or billed on it; a line added (an
  item, priced by the price list and pricing rules on a sale, or a description with its price); a
  line with nothing delivered, received or billed removed (quantity 0); the order's delivery or
  required-by date moved, with the lines that had it; and the price of what is not yet billed on a
  purchase order, or on a sales line without an item. Totals, taxes, status, percentages and
  reserved stock follow. Each change is one audit event (`update_items`) saying what changed from
  what to what. The changed order runs every app's checks as at submission (CRM's "an order keeps
  what was quoted", the countries', Finance's budget commitments, Procurement's requests and
  quotations).
- **Closing an order** keeps, on each line, what was still open (**Closed Short**) until it is
  re-opened; re-opening an order with nothing open is refused.
- **Allowances**: **Over-Delivery and Receipt Allowance (%)** and **Over-Billing Allowance (%)** on
  the item, else the company's. Within them a delivery, receipt or invoice goes through and the
  order line shows the excess; beyond them the refusal says what was ordered, what is allowed and
  what is asked. Returns give back what was actually delivered, excess included.
- **Credit control**: a customer's **Credit Limit** is checked on a sales order, a delivery note and
  an invoice, against open orders not yet billed, deliveries not yet billed and what is
  outstanding, each counted once (a delivery or invoice made from an order adds nothing). Over it,
  the document is refused, saying the limit, what is committed and what it adds; an Accounts
  Manager can fill in its **Credit Release Reason**, which is kept on it. **On Credit Hold**, with a
  reason, stops new orders, deliveries and orders growing; payments, credit notes and billing what
  was delivered go on.
- **Statement of Account** (report): a customer's or supplier's, in their currency, over a period:
  opening balance, each invoice, note, payment, refund, opening invoice and journal entry with a
  running balance, the closing balance and its ageing (current, 1-30, 31-60, 61-90, over 90); its
  company-currency closing agrees with the general ledger. Without a party, every customer with a
  balance. Price lists and item prices are in the Selling menu.

Refused, in words: a change to a closed, completed, cancelled or fully billed order; a quantity
below what was fulfilled or billed; removing a line with anything fulfilled or billed; a typed price
on a sales line with an item; a price change on a fully billed line; a change another app refuses.

Not done: a statement is not printed with the letterhead (the platform prints documents, not
reports); a change cannot be released over the credit limit; CRM counts what was ordered of a
quotation on submit and cancel only, so it does not follow a change until it implements
`after_save` (docs/contracts.md).

For data already there: nothing changes until someone sets an allowance, a credit limit or a hold.
The credit limit, checked on invoices before, now also counts open orders and unbilled deliveries,
and is checked on orders and deliveries. The new fields (`over_delivery_allowance`,
`over_billing_allowance` on items and companies; `credit_hold`, `credit_hold_reason` on customers;
`credit_release_reason` on orders, deliveries and invoices; `closed_qty` on order lines) reach a
workspace with its next schema update: no SQL migration. The interface is not rebuilt; its names in
Arabic and Hindi gain the new fields', translated by the model, to be reviewed by native speakers.

## ERP 0.17.0, India 0.3.1: documents that print like documents

- **A letterhead a company fills in once.** The company's form has three new sections:
  **Letterhead** (address lines, city, state or region, postal code, phone, e-mail, website and a
  **Logo**, an image uploaded to the company's files), **Bank Details on Documents** (account name,
  bank, account number or IBAN, and a branch, SWIFT, routing or IFSC code, as free text) and
  **Document Defaults** (default terms, for documents that carry none of their own, and a footer).
  Every layout below prints what is filled in and leaves out what is not: a company that has filled
  in nothing prints its name, as before. Where India's add-on holds the registered address
  (**Address** on the GST section), the layouts prefer the letterhead address and fall back to
  India's, so neither has to be typed twice.
- **Print layouts** for sales invoices (titled **Invoice**, **Credit Note** for a return and **Debit
  Note** for a supplementary invoice, each naming the invoice it is against, giving the reason of a
  note with no original, and marked **Price only** where no goods move), sales orders, delivery
  notes, purchase orders, purchase receipts, purchase invoices (the company's own record of the
  bill, a **Debit Note** when it is a return), payment entries (**Receipt**, **Payment Voucher**,
  **Refund** and **Transfer**, listing the invoices each settles, its deductions and what stays on
  account), journal entries (a voucher with debits and credits and their totals) and stock entries
  (titled by their purpose, with the warehouses that purpose has). The trade documents show the
  letterhead; the party with billing and shipping addresses and tax ID; number and dates; the
  customer's PO number or the supplier's invoice number; each line's name and description,
  quantity with unit, rate, discount where any and amount; every tax with its rate and amount (and
  the taxable amount per rate where the invoice has a breakdown), a note where prices include tax
  or a tax is reverse-charged; the totals before tax, discount, tax, rounding and the total, and on an invoice what was
  paid or credited and what is still due; the currency and, for a foreign one, the rate and the
  total in the company's currency; the party's payment terms; bank details on invoices and pro
  formas; terms; the footer; and a small **Draft** or **Cancelled** mark when the document is not
  submitted. A credit note, debit note or goods return prints the amounts credited or returned
  without minus signs. A line without an item prints its own description. They print on A4 or
  Letter with system fonts only, repeat a long invoice's table header on each page, never split a
  totals block, and read right to left where the text is Arabic.
- **Two ticks.** **Print as Pro Forma Invoice** on a sales order titles it Pro Forma Invoice, adds
  the bank details and says it is not a tax invoice. **Show Prices on the Print** on a delivery
  note prints rates, taxes and totals on it; without it a delivery note travels without prices.
- **India:** the GST tax invoice stays the invoice where India is installed and gains the
  letterhead's logo, contact line, bank details, default terms and footer; the letterhead's address
  is printed when filled in, the registered address when not.

What to fill in: the company's **Letterhead**, **Bank Details on Documents** and **Document
Defaults** (Company form). Not printed, because the platform's print templates cannot do it: the
amount in words, and the currency's symbol (the code is printed) and the precision of a company's
own currency on a stock entry (two decimals).

For data already there: nothing posted changes; documents print with the new layouts the next time
they are printed or saved as PDF, and look plainer where no letterhead is filled in. The new fields
reach an upgrading workspace with its next schema update: there is no SQL migration. They are named
`acc_address_line1`, `acc_city`, `acc_phone`, `acc_email`, `acc_logo`, `acc_bank_name`,
`acc_print_footer` and so on, on the company, for imports and the API. The interface is not rebuilt;
its names in Arabic and Hindi gain the new fields' (25 each), translated by the model, to be reviewed
by native speakers.

## ERP 0.16.0, Finance 0.4.0, India 0.3.0: no entry barrier for a company switching books

- **Lines without an item.** A sales or purchase invoice line, and a sales or purchase order's, may
  be a description, a quantity and a price booked straight to an account: rent, an electricity
  bill, a consultant's fee, a one-off charge. Leave **Item** empty: the line goes to the company's
  default income or expense account unless it names one, with its cost centre, and is taxed by the
  document's template (included in the price, compound, rounded per line, as any line). It takes no
  price list or pricing rule and moves no stock. Everything after treats it as a service item's
  line: totals and the ledger, part billing from an order, credit and debit notes against it
  (counted on its line, by its description when the note does not name it), credits of price
  alone, deferral over a service period, the sale mirrored in a group company's books. Registers,
  reports and e-invoices show its description. Deliveries and receipts keep items only: an order's
  lines without one are left off them, and an order of services alone is To Bill at once. India: a
  line without an item carries its own **HSN/SAC** code, needed on a GST-registered company's sales.
- **Opening invoices.** The invoices still open in the old books come in one record each (**Opening
  Invoice**: the party, the old invoice's number and date, its due date, the amount still open and
  its currency at the rate it is carried at; a negative amount for an open credit, an advance or an
  unused credit note), from a spreadsheet through the platform's import. **Post Opening Invoices** on
  the company posts every draft at once against its Opening Balance Account, all or none. Each is an
  open item under the number the party knows, aged from its own dates, that a payment settles as
  it settles an invoice made here: allocation, part payment, the exchange difference in another
  currency, the refund of an opening credit, Get Outstanding Invoices, and the Finance add-on's
  dunning.
- **A journal entry's party line** may give the number the party knows (**Invoice / Reference No**),
  its own **Due Date**, and the party's currency with its rate and the amount in it (the debit or
  credit is worked out), so opening balances entered the old way age by their real due dates and
  are settled in their own currency.
- **Transfers.** A payment entry of type **Internal Transfer** moves money between two of the
  company's bank and cash accounts, with no party, a date and a reference. Between two currencies,
  each side is in its account's currency; a rate not given is worked out from the two amounts, and
  given both, what the sides differ by is an exchange gain or loss as on any payment. It is cancelled
  like any payment; Finance reconciles it on both accounts' statements.

Refused, in words: a line without an item that does not say what it is, has no account to go to, or
is booked to a group, receivable, payable, bank, cash or stock account (naming the line);
an opening invoice of zero, in another currency without its rate, a number already carried in for
that party, dated after its opening date, or already in these books as a submitted invoice (a sale's
number, a supplier's invoice number); cancelling one while payments are allocated to it; another
currency on a journal entry line without a party or a rate; a transfer to the same account, to an
account that is not a bank or cash account of the company, or with allocations or deductions.

For data already there: nothing posted changes. Journal entries' party lines made before age on
their entry's day under its number, as before; payment entries made before, and those other apps
make with a type and a party, are Receive or Pay as before. Lines with an item are checked and
booked exactly as in 0.15.0, whatever account they name. On selling lines a price is typed on a line
without an item only: on a line with one it comes from the price list or a pricing rule, and a price
a person sends for a new item line, or for a line whose item changed, is not kept (the engine's own
documents, an order made from a quotation, keep theirs). India's
HSN/SAC on a sales invoice line is now copied from the item when the invoice is saved rather than
fetched, so an invoice keeps the code it was made with. The new fields and the Opening Invoice
entity reach an upgrading workspace with its next schema update: there is no SQL migration, and the
opening account is the company's Opening Balance Account, which the standard chart has (Create
Standard Chart of Accounts sets it on a company without one). Finance 0.4.0 needs ERP 0.16.0: its
reconciliation reads a transfer's receiving side, and its dunning lists opening invoices. The
interface is not rebuilt; its names in Arabic and Hindi gain the new fields' (640 each), translated
by the model, to be reviewed by native speakers.

## ERP 0.15.0: returns, credit and debit notes, and refunds

- **Create Return** on a submitted delivery note and purchase receipt, and **Create Debit Note** on
  a submitted purchase invoice (until now its debit note was a box to tick by hand), beside the
  sales invoice's **Create Credit Note**. Each makes a draft carrying the original's lines,
  prices, taxes, currency and rate, for what is still returnable; a returned delivery or receipt
  takes back what its order shows delivered or received.
- Each line of a return counts against the original's line it gives back, and the original shows
  per line what came back (**Returned Qty**) and, on invoices, what was given back before tax
  (**Credited**, **Debited**). A return beyond what is left is refused, naming the item and what is
  left; cancelling a return gives its quantity back. A return of a return, against a draft or a
  cancelled original, or (of a delivery or receipt) of an item the original did not move, is
  refused, and the original is not cancelled while returns stand against it. A note may still
  credit, against an invoice, an item billed on another (a subscription's seats billed on a
  supplementary invoice): that line counts against none of the invoice's.
- A credit of price alone: **Create Credit Note** (or **Create Debit Note**) with a share of the
  price makes a note that moves no goods, with its tax in proportion; what all the notes give back
  on a line never exceeds what was billed on it. A fixed charge (Actual) is given back in the share
  of the lines given back.
- A credit note, or a supplier's debit note, without its original invoice is accepted with a
  **Reason** (goodwill, a rebate, a correction from before these books): its taxes as entered, no
  quantities checked, and goods come back only with Update Stock, at the line's **Cost of Goods
  Returned** or their current valuation.
- Goods come back at the cost they left at: a credit note's on an invoice whose goods left on a
  delivery note at that delivery's cost (it took today's valuation), and a purchase return at the
  receipt's cost (FIFO items leave by their layers, as any issue does). Serial numbers and batches
  that come back must be ones the original moved.
- **Create Debit Note** on a sales invoice: an invoice charging more on it (a price increase,
  something charged short), linked to it, posted and counted against the credit limit as an
  invoice, and shown on the original (**Debit Notes**), which is not cancelled while it stands.
- Refunds: a payment entry with **Refund** ticked pays a customer, or receives from a supplier, and
  gives back their open credit (a credit note's balance, an
  overpayment, an advance), in part or in full, never more than is open and all of it allocated,
  in the credit's currency with the exchange difference booked as payments do; cancelling it
  restores the credit. **Refund** on a credit note or supplier's debit note with credit left makes
  that payment. A customer's refund debits the receivable and credits the bank.
- A full credit note mirrors its invoice to the cent (inclusive and compound taxes, a fixed charge,
  per-line and document rounding, cash rounding), in the invoice's currency at the invoice's rate:
  a return takes its original's currency and rate unless it names them, and one in another
  currency is refused.

For data already there: returns made before 0.15.0 are counted on their originals by the
migrations `selling/sql/001_returns.sql` and `buying/sql/001_returns.sql`, each old return line
tied to the first line of its original with its item. Payment entries made before, and those other apps
make, are unticked: a customer's receipt or a payment to a supplier, as before. A payment to a
customer without the tick is refused in words, never booked as a refund. Credit and debit notes have no menu entry of their
own: a menu lists entities and reports only, and the invoice lists filter on Is Return and Debit
Note. The interface is not rebuilt; its names in Arabic and Hindi gain the new fields' (594
each), translated by the model, to be reviewed by native speakers.

## 0.14.13

The reports it defines in code (their titles and parameters) have names in Arabic and Hindi too, with its models' and manifest's (558 each in ui/src/locales/names). Translated by the model, to be reviewed by native speakers.

## 0.14.12

Rebuilt with Ferrum-Frontend c59c9a2: the names of its models, fields and manifest in Arabic and Hindi (551 each in ui/src/locales/names, which the platform serves to every page of the workspace, so a record's labels, the launcher and the bar read in the person's language), and the sentences its pages still showed in English (split around a link or a figure, built in code from a template or word by word) whole and translated (383 words each in ui/src/locales). Translated by the model, to be reviewed by native speakers.

## 0.14.11

Rebuilt with Ferrum-Frontend 5b22e7b: its own words carry built-in Arabic and Hindi (317 words each, ui/src/locales, translated by the model and to be reviewed by native speakers), fetched for the page's language only. The labels it keeps in objects, its dialogs, its page titles and the words it keeps in variables go through t() as well, and the names and sizes it shows as they are keep their order right to left. Its pages are drawn once their words are in, and its entry carries only the parts of the kit it uses.

## 0.14.10

Rebuilt with Ferrum-Frontend 6537d65: an action's link, code or token shown once is kept in a dialog to copy, and on a page that reads right to left, English left untranslated keeps its own order. Its own words go through the kit's t() (289 places), so a workspace's own words for its language reach them, and Admin, Interface words lists them under it (dist/words.json).

## 0.14.9

Rebuilt with Ferrum-Frontend d5d98f2: a workspace's own words for its interface (Admin, Interface words) reach its pages as they load; an attachment field that holds a picture shows it; and a value another record already has is said so, where the form said the record had been changed by someone else. Its own screens' margins, paddings, borders, alignment and insets go by the line's start and end, so they mirror right to left as the kit's do; what it places by coordinates (boards, timelines, charts, handles) stays as it was.

## 0.14.8

Rebuilt with Ferrum-Frontend 63280d3: right to left, its pages mirror as they read (icons that point along the line, margins and alignment by the line's start and end, arrow keys along it), and the records pages' words are in Arabic and Hindi.

## 0.14.7

Rebuilt with Ferrum-Frontend a64416a: a new record's box whose default is 1 shows ticked, as it is saved.

## 0.14.6

Rebuilt with Ferrum-Frontend 922fcfb: an action's dialog says every refusal, the errors on what the action made (another record's rows) among them, and an action with parameters and a confirmation asks once.

## 0.14.5

Rebuilt with Ferrum-Frontend a76295e: its pages show times on a person's own clock when they chose one on their Account page, while today and due dates stay on the workspace's day.

## 0.14.4

Rebuilt with Ferrum-Frontend 24c3951: its pages show times on the workspace's clock, naming the zone when it is not the browser's, a draft numbered on submit is headed as a draft, and a record of an app that is not installed says so.

## 0.14.3

Rebuilt with Ferrum-Frontend bb60115: a record page reads an empty number or check as the server does, so an action whose condition needs one shows, and a workspace's settings open as their one record.

## ERP 0.14.2, Procurement 0.1.3, Finance 0.3.2: record pages' actions ask for what they need

Found walking procure to pay in a browser: a material request's Create Purchase Order was refused
("no usable approved supplier for Coffee grinder: choose a supplier for them") with nowhere to choose
one. Seven actions read parameters their metadata did not declare, so a record page could never send
them; two of them could not work from a page at all.

- A request for quotation's Create Supplier Quotation asks which invited supplier quoted (required;
  it was refused every time from a page).
- A bank line's Create Payment Entry asks for the customer who paid, or the supplier who was paid
  (`customer` and `supplier`; `party` still works); it was refused every time from its record page.
  Create Journal Entry asks for the account and a remark.
- A material request's Create Purchase Order asks for a supplier for the lines whose item has none
  approved; Create Stock Entry for where the stock comes from; Create Work Orders for where the
  materials come from.
- Import Bank Statement (CSV) is no longer a button on the bank account's page, which could not send
  a statement: the Import screen does that.
- A purchase invoice made from an order or a receipt holds its draft's place with
  `PENDING-<number>` as the supplier's invoice number; submitting it with that is refused ("Enter
  the supplier's own invoice number"). In the walk a typed number was appended to it and the bill
  went in as `PENDING-PR-2026-00001GC-2026-501`.
- Also in this release: `docs/contracts.md` publishes what apps that post through the engine may rely on: the ledger's,
  open items', stock's, pricing's and the countries' operations, each in the caller's transaction,
  with what it refuses and whether calling it twice does it twice. `tests/contract_it.rs` holds
  each guarantee: a posting balances or writes nothing, a reversal reverses once, an invoice's item
  opens once and takes no more than it is owed, stock is not issued beyond what is there.

## ERP 0.14.1: rebuilt with Ferrum-Frontend 00dea07

- In a workflow state that names what may change, the other fields show read-only, and the fields
  the state needs show required.

## ERP 0.14.0: a subscription's revenue over the days it pays for, and MRR as plans stood

- A subscription invoice's lines carry the days they bill as their service period, so a plan whose
  item defers its revenue bills (it could not: a deferred line needs its period) and is earned day
  by day over the period. A cancellation or a smaller plan gives the unused days back on a credit
  note deferred over those same days, so what is earned is what was used, month by month, rather
  than all of the refund on its day.
- A credit note's deferred lines are recognised like their invoice's, negative, over the days they
  credit (ERP 0.9.0 refused them: a refund reversed income on its date while the invoice went on
  earning). The deferred revenue and expense report lists them too.
- A line is deferred like its item when it is added with the days its service covers; afterwards
  its own setting stands. Until now an item's deferral never reached a line, since an unset
  checkbox already reads as unchecked when prices are worked out.
- Subscription metrics read each subscription's MRR as its plans stood on the day: changes of
  plans are recorded from the day they take effect (a change dated before a later one replaces
  it). The report adds expansion and contraction, so start, new, expansion, contraction and churn
  take the start to the end, and net revenue retention.

## ERP 0.13.0: a sale within the group, in the buyer's books too

- A sales invoice to a customer that represents one of the group's companies says so (**Group
  Company**), and **Create Purchase Invoice in the Group Company** makes that company's purchase
  invoice from it: from the supplier that represents the seller, with the sale's number and day as
  the supplier's invoice number and date, at the sale's net prices; the buying company's own taxes
  and accounts apply. It is made once, and the sale is not cancelled while the purchase invoice
  stands (FERRUM-ERP-005).

## ERP 0.12.0: non-controlling interests and a group's cash flow

- A company can say how much of it its parent holds (**Held by the Parent (%)**, 100 unless set).
  The group's owners hold a company through the chain above it, and the rest is
  **non-controlling interests** (IFRS 10): the consolidated balance sheet splits its equity into what
  is attributable to the owners of the parent and the non-controlling interests, and the
  consolidated profit and loss its profit the same way. Each company's share held outside the group
  is taken of its own net assets at the closing rate and of its own profit at the rates of its days;
  a company's investments in the group's companies are left out of its net assets, so that a chain
  is not counted twice. `meta.non_controlling_interests` shows how each company's part is made up.
  The group's totals are unchanged (FERRUM-ERP-005).
- The **cash flow** of a group (`consolidated`): every flow translated at the rate of its day,
  business within the group left out, each company's own bank and cash accounts as its cash, at the
  rates of either end, and what that leaves the effect of exchange rates on cash (IAS 7). Business
  eliminated only in part shows as a line of its own, so the statement ends where the group's cash
  does.

## ERP 0.11.0: a group's books translated as IAS 21 says

- A group's consolidated trial balance, profit and loss and balance sheet translate a company in
  another currency as IAS 21 does (FERRUM-ERP-005): its assets and liabilities at the closing rate,
  its income, expenses and equity at the rate of their own day where the books have one (the latest
  on or before it, as a document takes its rate), else at the closing rate. A group that keeps only
  closing rates sees the same figures as before.
- What translating at different rates leaves is the **Foreign currency translation reserve**, an
  equity line of its own on the consolidated trial balance and balance sheet (credit when the
  closing rate made the company's net assets worth more than the rates its income and equity were
  taken at). It is not profit: the consolidated profit and loss leaves it out. The intercompany
  difference is again only what was eliminated in part.

## ERP 0.10.2 and Finance 0.3.1: a reversal day worked out when it is submitted

- A journal entry that reverses itself, and an exchange revaluation, take "the day after" as their
  reversal day when they are submitted, from their day then. It was worked out while the document
  was a draft, so the form's preview filled it from the first date given and kept it when the date
  changed: an accrual drafted on 5 November and moved to 31 October would have reversed on
  6 November. Found walking an accrual in a browser. A reversal day someone chooses is kept as before.
- Interface rebuilt with Ferrum-Frontend a110af4: an action says what it did ("1 deferred line
  recognised through 2026-10-31") rather than "done".

## ERP 0.10.1: the standard chart reaches companies made before it grew

- **Create Standard Chart of Accounts** on a company that already has accounts now adds the
  standard accounts it lacks, under their standard parents where the company has them (without
  the standard number when another account holds it), and sets the default links still empty.
  Nothing the company has is changed. A company made before 0.9.0 had no Deferred Revenue or
  Prepaid Expenses, so a deferred line was refused; walking deferred revenue in a browser found
  it, and the refusal points at this action.

## ERP 0.10.0: accruals that reverse themselves, and what remains deferred

- A journal entry may reverse itself, as an accrual or a prepayment does (FERRUM-ERP-006): marked
  **Reverse It**, it is taken out again on its reversal day, the day after unless another is
  chosen, when the invoice or payment it stood in for is booked. The daily upkeep reverses it on
  that day, once (locked, so two replicas at once reverse it once), and one whose day's period is
  closed waits for the next round; one whose day has already come is reversed as it is
  submitted. Cancelled after its reversal, nothing more moves. Such an entry books to accounts only: what a
  customer or supplier owes comes with their invoices. The design is the exchange revaluation's,
  which already reversed itself the day after.
- **Deferred Revenue and Expense**, a report of every deferred line of a company's submitted
  invoices (sales or purchase): its service period, its amount, what was recognised and through
  when, what still waits on its deferred account (which is what the account holds once
  recognition has run), and what recognising through a day would move.
- The standard chart has Accrued Income (1180) and Accrued Expenses (2160).

## ERP 0.9.0: revenue and expense over a service period

- Deferred revenue and expense (FERRUM-ERP-006), as ERPNext defers them: an invoice line marked
  deferred, with the days its service starts and ends, books to a deferred revenue account (a
  liability) or a deferred expense account (an asset) instead of its income or expense account.
  Recognition moves what each day of the period earned or used to the line's own account, under
  the invoice, so cancelling the invoice reverses what was recognised with the rest; the last day
  takes what rounding left. It runs for every company through the end of last month, and the
  company's **Recognise Deferred Revenue and Expense** recognises through a day an accountant
  chooses (not one still to come). Each line shows what was recognised and through when. Two
  runs at once (on two replicas, or an accountant's and the monthly one) recognise each day once:
  each invoice is locked while its lines are recognised.
- An item may be deferred by default (Deferred Revenue, Deferred Expense); a line is deferred like
  its item unless it says otherwise. A deferred line needs its service period; a return and a
  stock item are not deferred.
- The standard chart has Deferred Revenue (2150) and Prepaid Expenses (1170), the company's
  default deferred accounts.
- Your orders says "Billed, to deliver" of an order billed in full before it was received (it read
  "To deliver"), found walking the supplier portal.

## ERP 0.8.0, Procurement 0.1.2, Warehouse 0.2.1 and Shop Floor 0.2.1: a supplier's orders on the portal

- **Your orders**, a page of the workspace's portal for suppliers (FERRUM-ERP-011): someone whose
  address is a supplier's, or one of its contacts', sees the purchase orders sent to that supplier
  (their dates, when they are required, how much was received and billed, and the order as the
  workspace prints it, in PDF too) and the bills ERP holds from them with what is still owed and
  when it is due. Drafts and cancelled orders are not theirs to see; another supplier's are not
  found. It is the buying package's site, `/api/app/buying/portal/orders` and `…/bills`, beside
  the selling package's "Your invoices", in the same interface release.
- With Procurement, the page also lists the requests for quotation waiting for the supplier's
  answer before their deadline, each with the link the supplier was e-mailed (signed alike, so
  the same page); answered, a request leaves the list (`/api/app/procurement/portal/rfqs`).
- ERP's interface and the Warehouse and Shop Floor interfaces are rebuilt with Ferrum-Frontend
  d4efeca: a field's label names its control for assistive technology, and a record whose title
  is a link shows that record's title, not its id.

## India 0.2.2: e-invoices checked against the IRP's own rules

- The e-invoice tests apply the IRP's published value checks (its developer portal's Generate IRN
  validations, read on 5 November 2026: item and invoice sums, CGST and SGST within the state,
  IGST otherwise, cess, the round-off's range and the ±₹1 tolerance with the portal's three
  examples) to every kind of e-invoice ERP builds: within the state, to another state, under
  reverse charge, to an SEZ unit, exports with and without IGST, a credit note, and lines in
  paise with a rounded total (FERRUM-VERIFY-002).
- What they found: an invoice to another state or an SEZ unit carrying CGST and SGST was sent as
  it was, and the IRP refuses it; it is now refused here, naming the IGST template. IGST charged
  within the state is marked `IgstOnIntra`, as the IRP asks.

## ERP 0.7.1: what the assistant knows in ERP's pages

- A `shell.assistant` contribution (FERRUM-AI-006): asked from one of ERP's pages, the assistant
  knows what a submitted, draft or cancelled document means, where what customers and suppliers owe
  comes from, that the reports hold the books' figures (and which), and what finance books and
  groups are; it suggests five questions to start with. Needs a Ferrum that knows the point (#49).

## ERP 0.7.0: a group's books

A company can have a parent; a parent and every company under it are a group, and its books add up
(FERRUM-ERP-005).

- A Consolidated Trial Balance, and a Consolidated choice on the profit and loss and the balance
  sheet: the group's companies in the parent's currency, each at its latest exchange rate on or
  before the end date, on the parent's chart (an account by its number, else its name; in the
  statements an account the parent lacks shows on the nearest one above it that it has).
- The group's business with itself is taken out: what a customer or supplier that represents one of
  its companies owes or is owed, and lines on accounts marked Eliminate in Consolidation (intercompany
  sales and costs, investments in a subsidiary and its capital) unless their entry is with someone
  outside the group. A parent's own equity stays.
- What does not cancel shows: an entry taken out only in part leaves an intercompany difference, and
  what two companies say they owe each other is set side by side, with how far they disagree.
- Adding up a group takes being allowed to see each of its companies.

Not yet: income and expense at each day's rate with a translation reserve (IAS 21: everything is at
the closing rate, as ERPNext does), ownership percentages and minority interest, consolidating the
cash flow, and an intercompany invoice mirrored into the other company.

## ERP 0.6.0 and Finance 0.3.0: finance books

Books kept side by side, as ERPNext's finance books keep them: IFRS beside local rules, a tax book
beside both.

- A journal entry with no finance book is in every book; one with a book adjusts that book alone. A
  book is the common ledger and its own entries, and balances: the database checks that all of an
  entry's lines are in its book.
- A book's entries adjust its valuations. They may not touch receivables or payables: what customers
  and suppliers owe is the same in every book.
- Trial balance, general ledger, profit and loss, balance sheet and cash flow take a Finance Book and
  say which they show. With none they show the common books, as before: nothing changes for a
  company without books.
- Cancelling a book's entry reverses it in that book. The year-end closing closes every book, each
  into an entry of its own, so each book's retained earnings carry its own result.
- Budgets, exchange revaluation and the finance overview read the common books.

Not yet: assets depreciating differently per book (Assets), and consolidation across companies.

## Warehouse 0.2.0 and Shop Floor 0.2.0: what holds when things repeat, arrive late or go wrong

From a review of the connected system, against a release, of what happens under retries, crashes and
physical exceptions. **Nothing has touched physical equipment, and none of this is a safety claim.**

Warehouse:

- Releasing a task twice is one mission. A lost answer gets the first mission back, two requests at
  once write one, and the ledger's own index refuses a second live command for a task. A new attempt
  comes only from a mission that never started. Handing a task on its way to someone else is refused.
- A move proves what moved. Confirming takes a scan of the load as well as the place: a handling unit's
  label, or the item with its lot and serial. **Anything confirming a task now has to send these**
  (`unit_scan`, or `item_scan`, `lot_scan`, `serial_scan`). A manager can finish one that cannot be
  scanned, with a reason; it stays on record as `Manual`, and every task says what established it.
- A refused scan stays on record. The audit was being written in the transaction that was then rolled
  back, here and at packing.
- A scan used once cannot finish another task, where it used to mark it done with nothing moved.
- What a lost mission was bringing is still owed. Found on the way, a pick or a line's supply goes on
  from there as a new task, and found where it was going it has arrived. A pick ended with a wave
  saying Picked and nothing to do, and a run's material found at the line was not counted. What
  nobody found, or a person gave up on, is written on the wave's shortfall. A shipment nothing reached
  closes.
- Documents can be taken back in a managed warehouse. Cancelling a receipt looked for its stock at the
  line and could not succeed, and neither could cancelling an issue. A receipt now goes back out by the
  inbound bin, if it is still there, and an issue comes back in by it.
- A robot's short load is not a short pick: what did not arrive stays out of reach until someone finds
  it. A mission turned down after it was taken ends in someone looking, not on its way for ever. A
  report a task cannot take no longer retries for ever. A task at a site's station is finished there.
- Held stock cannot reach the line, and a task or run that finishes by recovery or cancellation now
  moves its wave and its run on.

Shop Floor:

- A machine starts only once its material is at the station. Releasing a run sets its material aside,
  so a second run cannot be released on the same stock, and the warehouse's progress moves the run from
  Waiting through Reserved to At Station. With no warehouse, a supervisor says so, on the record.
- Every prerequisite is asked again at that moment. A hold stops a start that has not reached the
  machine, and one that has is asked to stop.
- A machine already loaded says so on the run (`material_source: Preloaded`), and whoever releases it
  answers for that.
- Material said to have gone in can be taken back. A wrong lot stopped every report after it, with no
  way out. What has been posted stays posted, and both events stay on the run.
- **`machine.start_job` must be marked as kept to its app on the device**, or a run on it is refused:
  a start that can be asked for around these gates is not behind them.

## Warehouse 0.1.0, Shop Floor 0.1.0 and Quality 0.1.0: from the dock to the machine

Three add-ons that run on ERP's stock and work orders, and on Operations from the platform. They have
been run end to end on a real server with a simulated fleet and a simulated press. **Nothing has
touched physical equipment, and none of this is a safety claim.**

- **Warehouse** runs a warehouse down to the bin. Positions are a subledger of ERP's stock, never a
  second balance: the database refuses any commit where the two differ. Receiving by scan, putaway,
  wave picking, packing, partial shipments by the parcel, counts with a second counter, returns,
  holds, and storage billing for customers' stock. A robot's arrival moves nothing: stock moves on a
  scan at the destination, and when nobody can say where it is, the task says so until someone finds it.
- **Shop Floor** runs work orders on the floor. A run is released only with an approved revision, a
  qualified operator, equipment that can run and material there, and releasing it brings that material
  to the line. Events are idempotent, so a shaky terminal counts once. Lots trace forward and back,
  and OEE follows a written definition that says when a figure is unknown rather than zero. It sends
  machines an approved job number, never a program.
- **Quality** puts holds that stop shipping and consuming held stock, in ERP, the warehouse and on the
  floor, with nonconformances and corrective action. Whoever placed a hold does not release it.

## ERP 0.5.0 and Finance 0.2.0: the accountant's screens

ERP's own interface now has hand-made screens for the accountant's work, each with its rules in the
server and its figures worked out by hand in the tests.

- **Bank reconciliation** (Finance, in the sidebar): a bank account's statement lines beside what
  the books have for the selected one, with the bank's balance after each line. The statement's
  balance, the books' and the difference are always in view, with what makes the difference up.
  Exact matches are paired so that no payment and no line is used twice; `j` and `k` walk the
  lines, `Enter` accepts the match, `Shift+A` settles every exact match after showing them, and a
  line can be matched with several entries (with amounts you edit), turned into a payment or
  journal entry, or undone. A statement file is checked before it is imported: its columns are
  guessed, and the server's own trial run counts the new lines and the ones the account already has.
  New in the server: the account's `workbench` and `accept_matches` actions, and a `dry_run` for
  the statement import.
- **Payment allocation** (Finance, New Payment Allocation): a customer's or supplier's payments,
  credit notes and journal credits applied to their open invoices in one document, oldest invoices
  first with one button, with what is left over on both sides always shown. Each pairing's
  exchange gain or loss is worked out by the server before anything is posted. The document
  applies every pairing or none, and cancelling it opens them again; the payments, invoices and
  journals it applied cannot be cancelled before it.
- **Financial statements**: the profit and loss and the balance sheet can be set beside up to three
  earlier periods (the period before, or the same one a year back), each row with how much it moved
  and by what share; the profit and loss can be narrowed to a cost centre (with the ones below it)
  or a project. The new cash flow statement (indirect method) sorts what moved in the other
  accounts into operating, investing and financing by the account's type or by a new Cash Flow
  Section on the account (the standard chart marks its loans as financing), and must end where the
  bank and cash accounts did. The general ledger opens a group account as every account below it
  and can leave out the year-end closing, so any figure opens the entries behind it.
- **Exchange revaluation** (Finance, New Exchange Revaluation): at a period's end, each foreign
  currency balance (bank and cash accounts, each customer's receivable and supplier's payable,
  accounts kept in another currency) is carried at the day's rate and the difference goes to the
  exchange gain/loss account in one entry. As the gain is not realised, it is reversed the day after
  (or a day chosen; the daily upkeep posts a reversal whose day has not come), and cancelling takes
  it out at once. A day with no exchange rate is refused with the reason.
- **Accounts today** (Finance Overview, and a line on ERP's home for those who keep the books):
  overdue and soon-due receivables and payables, bank lines to reconcile, drafts waiting, months
  and years that have ended and are not closed (an Accounts Manager closes a month from there),
  and the cash there is.
- Not in this version: deferred revenue and expense (the invoice lines would have to post to a
  deferred account and a schedule recognise it), and dimensions beyond cost centre and project.

## ERP 0.4.1: tax rates as data

- Tax Rate (Accounting): a country's rate for a tax category from a day (and, optionally, to a
  day, for a state or province), kept as a record. Localizations prefer it to their built-in
  rates (`localization::rates`), so a new rate needs no release: the thirteen countries of
  `ferrum-countries` read it now. Codes are checked (ISO 3166-1 capitals), rates are at most 100%
  and cannot end before they start.

## Portugal 0.1.0

- Portugal (`ferrum-eu-members`, `pt`), on the European Union's add-on: invoices from certified
  software. Each invoice takes the next number of a series communicated to the AT (PT Series:
  FT, FS, FR, NC, ND, with the AT's validation code) and its ATCUD; it is signed (RSA, SHA-1,
  Despacho n.º 8632/2014) over date, system entry (Lisbon time), number, gross total and the
  series' previous signature, so the series forms a hash chain; it carries the four hash
  characters with the certificate's mention and the QR code's message (Portaria 195/2020: A to S,
  VAT by band, consumer NIF 999999990); cancelling marks it A. NIFs are checked; the small
  business exemption (article 53) and the construction reverse charge carry their legal texts.
  The producer's key and certificate number come from `FERRUM_PT_SIGNING_KEY` (PEM),
  `FERRUM_PT_CERTIFICATE` and `FERRUM_PT_KEY_VERSION`; without them submitting refuses.

## Romania 0.1.1

- Romania's connection to ANAF's e-Factura (`ferrum-eu-members`, `anaf`): Connect to ANAF starts
  ANAF's OAuth sign-in with the company's qualified certificate (`logincert.anaf.ro`); ANAF returns
  the user to `/api/app/ro/anaf/callback`, which checks the state it issued and takes the tokens
  (sealed, refreshed when they lapse). Queued CIUS-RO invoices are uploaded to e-Factura (test or
  production, `upload?standard=UBL&cif=`), ANAF's upload index recorded, and ANAF asked after them
  (`stareMesaj`) until it says ok (with the download ID of the signed invoice) or nok (with its
  errors). With the connection Off, invoices wait for their answer to be recorded by hand.

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
