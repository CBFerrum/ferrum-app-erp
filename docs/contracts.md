# ERP's contract with other apps

Apps build on ERP in two ways, and this is what each may rely on. `tests/contract_it.rs` holds
every guarantee below that it can state as a check; `tests/erp_it.rs` walks the documents.

## Through documents

Most apps invoice, pay and move stock by creating and submitting ERP's documents through the
platform (`SalesInvoice`, `PurchaseInvoice`, `PaymentEntry`, `DeliveryNote`, `StockEntry`,
`JournalEntry` and the rest): Hospitality's stays, Healthcare's visits, Property's rent, Logistics'
deliveries. Their contract is each entity's definition under `packages/` (its fields, what is
required, its workflow and actions) and the platform's: validation, permissions, the audit trail,
the records API's idempotency keys. A document submits once; cancelling it undoes its effects.

Returns and refunds, since 0.15.0: a return (`is_return` with `return_against`) gives back what
its original's lines have left (`returned_qty`, and on invoices `credited_amount` before tax), and
a line names the original's line it gives back in `return_line` (the first with its item when it
does not). A credit note without an original needs `return_reason`. A `PaymentEntry` with
`is_refund` ticked pays a customer or receives from a supplier, refunding their credit: its
`references` are the party's negative open items, all of the money allocated to them. Unticked (as
every payment made before, and every one sent with only `payment_type` and a party), it is a
customer's receipt or a payment to a supplier, as before.

Since 0.16.0: a line of `SalesInvoice`, `PurchaseInvoice`, `SalesOrder` and `PurchaseOrder` may
have no `item`: it then needs a `description` (copied to `item_name`), its `price_list_rate`, and on
invoices an `income_account` or `expense_account` (the company's default when not sent). Such a line
takes no price list or pricing rule, moves no stock, is left off deliveries and receipts made from
its order, and is refused (naming the line) with no description, no account to go to, or an
account that is a group, a receivable, payable, bank, cash or stock account. Lines with an item are
checked and booked as before. On selling documents a person's `price_list_rate` is kept on a line
without an item only; on a line with one it comes from the price list or a pricing rule, and is kept
as sent only on the engine's own (elevated) writes, as before. Readers of invoice lines (`items.item`) must allow it to be empty.
`PaymentEntry` has a third `payment_type`, `Internal Transfer`: no party, `bank_account` to
`paid_to`, `received_amount` in the receiving account's currency; payments sent with `Receive` or
`Pay` and a party are as before. An open item's `entity` may be `OpeningInvoice` (an invoice
carried in from the books before, its `number` the old one), and a `JournalEntry` party line's
item carries the line's `reference_no`, `due_date` and `currency` when given;
`flow::sync_invoice_outstanding` also mirrors an `OpeningInvoice`.

## Through the engine

Apps that post their own documents' effects (Point of Sale's sales, Commerce's orders, Assets'
depreciation, HR's payroll, Field Service's jobs) call the engine as a Rust crate. For these
operations:

* **The caller's transaction.** Each takes the caller's `&mut Tx` and `Ctx` and does nothing of
  its own: what it writes commits or rolls back with the caller's document action. An `Err` is
  returned to the platform, which rolls the whole action back, so nothing half done survives.
* **Append only.** Postings, moves and allocations are never changed or deleted; undoing appends
  reversals, and the database refuses edits, deletes and postings into a closed period.
* **Once.** A posting appends on every call: call it once for each event of the source document
  (its submit, its cancel), which the platform's workflow runs once. Reversals act only on what is
  not reversed yet, so calling one twice reverses once.
* **Typed.** The signatures are the contract, and the compiler checks them when an app builds
  against an ERP revision. ERP is 0.x: a minor version may change one of them, and says so under
  *Breaking* in the changelog. Anything public that is not listed here is ERP's own.

| Operation | What it does | What it refuses | Called twice |
|---|---|---|---|
| `ledger::post(tx, ctx, Entry, precision) -> Option<Uuid>` | Merges lines by account and dimensions, rounds them, and appends one hashed journal entry; `None` when they net to nothing | `Error::Internal` when debit and credit differ after rounding; the database, a closed period | Two entries |
| `ledger::reverse(tx, ctx, entity, id, fallback, precision) -> usize` | Appends a reversal of each entry of the source not reversed yet, on its own date or `fallback` when that period is closed; returns how many | | Reverses once |
| `ledger::reverse_on(tx, ctx, entity, id, on, precision) -> usize` | The same, every reversal dated `on` | The database, when `on`'s period is closed | Reverses once |
| `ledger::open_item(tx, ctx, &OpenItem)` | Opens the receivable or payable of an invoice, whose id it takes; nothing for a zero amount | An item already open with that id | Refused |
| `ledger::allocate(tx, ctx, company, item, payment_entity, payment_id, on, amount, base_amount)` | Settles part of an open item with a payment; a credit (a negative item) takes a negative amount, as a refund gives it back | `Error::Invalid` beyond what is outstanding, against its sign, or on a cancelled item | Two allocations |
| `ledger::unallocate(tx, ctx, payment_entity, payment_id, on) -> Vec<Uuid>` | Reverses what a payment allocated, restoring what is outstanding; returns the items | | Undoes once |
| `ledger::cancel_open_item(tx, ctx, id)` | Cancels an item: nothing outstanding, no payment taken | `Error::Invalid` while payments are allocated to it | Stays cancelled |
| `ledger::load_open_item(tx, tenant, id, lock) -> Option<OpenItemRow>` | Reads an item, locked for the transaction when asked | | |
| `stock::lock_balances(tx, tenant, company, keys)` | Locks the balances a document will touch in a fixed order, so concurrent postings cannot deadlock | | Re-entrant |
| `stock::post_moves(tx, ctx, &Company, entity, id, date, ts, moves, serial_status) -> Vec<Posted>` | Values each move from the balance at posting time (moving average or FIFO) under its lock, and appends it | `Error::Invalid`: not a stock item, an unknown, group or other company's warehouse, a zero quantity, serials or a batch missing or not there, a negative rate, more than is in stock unless the company allows negative stock | Two moves |
| `stock::reverse_moves(tx, ctx, &Company, entity, id, date, ts) -> Vec<Posted>` | Exact reversals of the source's moves not reversed yet; a valuation difference goes to the stock adjustment account | | Reverses once |
| `stock::valuation_rate(tx, tenant, company, item, warehouse) -> Decimal` | An item's current rate in a warehouse, else in any of the company's | | |
| `stock::parse_serials(Option<&str>) -> Vec<String>` | Serial numbers from text, one per line or comma | | |
| `pricing::calculate(k, ctx, tx, &EffEntity, &mut Doc, Side) -> Company` | Fills a priced document's prices, discounts, item tax rates, taxes and totals, after the installed countries choose its taxes | Field errors: no company, no posting date, what the countries refuse | The same document, the same figures |
| `pricing::calc_input`, `tax_in`, `line_tax_rates`, `tax_breakdown`, `breakup` | The calculation's parts over a document's lines, without reading anything | | |
| `localization::check(hc, doc, Side)` | Each installed country's checks before a priced document is submitted: call it in `before_submit` | What the countries refuse | Reads only |
| `flow::sync_invoice_outstanding(k, ctx, tx, entity, invoice)` | Mirrors an open item's outstanding amount and status onto its `SalesInvoice`, `PurchaseInvoice` or `OpeningInvoice` | | Sets the same values |

Not in it: closing periods, reports, set-up, the trade documents' own hooks and everything under
`addons/`. They are ERP's, and apps reach them through documents and reports.
