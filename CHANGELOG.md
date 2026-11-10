# Changelog

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
- Refunds: a payment entry names its party (**Party Type**), so it can pay a customer and receive
  from a supplier. One the other way gives back their open credit (a credit note's balance, an
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
tied to the first line of its original with its item. Payment entries made before keep their party
from Receive (a customer) or Pay (a supplier). Credit and debit notes have no menu entry of their
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
