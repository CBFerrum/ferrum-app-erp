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

Returns and refunds: a return (`is_return` with `return_against`) gives back what
its original's lines have left (`returned_qty`, and on invoices `credited_amount` before tax), and
a line names the original's line it gives back in `return_line` (the first with its item when it
does not). A credit note without an original needs `return_reason`. A `PaymentEntry` with
`is_refund` ticked pays a customer or receives from a supplier, refunding their credit: its
`references` are the party's negative open items, all of the money allocated to them. Unticked, it is a
customer's receipt or a payment to a supplier.

A line of `SalesInvoice`, `PurchaseInvoice`, `SalesOrder` and `PurchaseOrder` may
have no `item`: it then needs a `description` (copied to `item_name`), its `price_list_rate`, and on
invoices an `income_account` or `expense_account` (the company's default when not sent). Such a line
takes no price list or pricing rule, moves no stock, is left off deliveries and receipts made from
its order, and is refused (naming the line) with no description, no account to go to, or an
account that is a group, a receivable, payable, bank, cash or stock account. Lines with an item are
checked and booked from their item. On selling documents a person's `price_list_rate` is kept on a line
without an item only; on a line with one it comes from the price list or a pricing rule, and is kept
as sent only on the engine's own (elevated) writes. Readers of invoice lines (`items.item`) must allow it to be empty.
`PaymentEntry` has a third `payment_type`, `Internal Transfer`: no party, `bank_account` to
`paid_to`, `received_amount` in the receiving account's currency; `Receive` and `Pay` take a party. An open item's `entity` may be `OpeningInvoice` (an invoice
carried in from the books before, its `number` the old one), and a `JournalEntry` party line's
item carries the line's `reference_no`, `due_date` and `currency` when given;
`flow::sync_invoice_outstanding` also mirrors an `OpeningInvoice`.

A submitted `SalesOrder` or `PurchaseOrder` may change through its `update_items`
action. The changed order runs every hook registered on the entity as at submission
(`before_validate`, `validate`, `validate_change` with the order as it was, `before_submit`; refusing
there refuses the change), is stored, and then runs `after_save` with the order as it was (both
submitted). An app that counts an order's lines in `on_submit` and `on_cancel` moves its counts in
`after_save` when the old order is submitted, as Procurement does; one that does not keeps the counts
of the order as submitted. A closed order's lines carry `closed_qty`. Progress on an order line
(`delivered_qty`, `received_qty`, `billed_qty`) may pass its `qty` within the item's or company's
allowance. Submitting a `SalesOrder`, `DeliveryNote` or `SalesInvoice` may be refused for the
customer's credit (limit or hold) unless it carries a `credit_release_reason`.

The company's read-only capability `inventory.company.scan` (`{text}`) says what a scanned text is:
`{text, gs1, matches, ambiguous}`, each match `{kind, entity, id, title, item, item_name, uom, qty, batch_no, expiry,
serial_no, warehouse}` with `kind` one of `item` (by barcode or code), `gs1` (an element string's item by its GTIN,
with its batch, expiry, serial and quantity), `batch`, `serial`, `bin` or `document` (by its number; a carrier
shipment by its tracking number too). Read with the caller's rights, at most 20 matches, a text of at most 200
characters; `ambiguous` when more than one record answers, nothing when none does. The unit is the item's stock unit.

## Through the engine

Apps that post their own documents' effects (Point of Sale's sales, Commerce's orders, Assets'
depreciation, HR's payroll, Field Service's jobs) call the engine as a Rust crate. For these
operations:

* **The caller's transaction.** Each takes the caller's `&mut Tx` and `Ctx` and does nothing of
  its own: what it writes commits or rolls back with the caller's document action. An `Err` is
  returned to the platform, which rolls the whole action back, so nothing half done survives.
* **Append only.** Postings, moves and allocations are never changed or deleted; undoing appends
  reversals, and the database refuses edits, deletes, postings into a closed period and postings on a day no
  accounting period holds (a company has its current fiscal year from set-up; make the others before posting in them).
* **Once.** A posting appends on every call: call it once for each event of the source document
  (its submit, its cancel), which the platform's workflow runs once. Reversals act only on what is
  not reversed yet, so calling one twice reverses once.
* **Typed.** The signatures are the contract, and the compiler checks them when an app builds
  against an ERP revision. ERP is 0.x: a minor version may change one of them, and says so in the
  changelog. Anything public that is not listed here is ERP's own.

| Operation | What it does | What it refuses | Called twice |
|---|---|---|---|
| `ledger::post(tx, ctx, Entry, precision) -> Option<Uuid>` | Merges lines by account and dimensions, rounds them, and appends one hashed journal entry; `None` when they net to nothing | `Error::Internal` when debit and credit differ after rounding; a dimension value that is not one, or an account's required dimension left empty; the database, a closed period or a day no period holds | Two entries |
| `ledger::reverse(tx, ctx, entity, id, fallback, precision) -> usize` | Appends a reversal of each entry of the source not reversed yet, on its own date or `fallback` when that period is closed; returns how many | | Reverses once |
| `ledger::reverse_on(tx, ctx, entity, id, on, precision) -> usize` | The same, every reversal dated `on` | The database, when `on`'s period is closed or there is none | Reverses once |
| `ledger::open_item(tx, ctx, &OpenItem)` | Opens the receivable or payable of an invoice, whose id it takes; nothing for a zero amount | An item already open with that id | Refused |
| `ledger::allocate(k, tx, ctx, company, item, payment_entity, payment_id, on, amount, base_amount) -> Decimal` | Settles part of an open item with a payment; a credit (a negative item) takes a negative amount, as a refund gives it back. A settlement that clears what is outstanding takes all the item has left in the company's currency, whatever `base_amount` says, and every call returns the base it took: post that, and any difference from the money's own value as an exchange difference. The item's invoice then shows what is left on it (`flow::sync_invoice_outstanding`) | `Error::Invalid` beyond what is outstanding, against its sign, or on a cancelled item | Two allocations |
| `ledger::unallocate(k, tx, ctx, payment_entity, payment_id, on) -> Vec<Uuid>` | Reverses what a payment allocated, restoring what is outstanding and what the items' invoices show; returns the items | | Undoes once |
| `ledger::cancel_open_item(tx, ctx, id)` | Cancels an item: nothing outstanding, no payment taken | `Error::Invalid` while payments are allocated to it | Stays cancelled |
| `ledger::load_open_item(tx, tenant, id, lock) -> Option<OpenItemRow>` | Reads an item, locked for the transaction when asked | | |
| `stock::lock_balances(tx, tenant, company, keys)` | Locks the balances a document will touch in a fixed order, so concurrent postings cannot deadlock | | Re-entrant |
| `stock::post_moves(tx, ctx, &Company, entity, id, date, ts, moves, serial_status) -> Vec<Posted>` | Values each move from the balance at posting time (moving average or FIFO) under its lock, and appends it | `Error::Invalid`: not a stock item, an unknown, group or other company's warehouse, a zero quantity, serials or a batch missing, not there, expired or disabled, a negative rate, more than is in stock unless the company allows negative stock | Two moves |
| `stock::reverse_moves(tx, ctx, &Company, entity, id, date, ts) -> Vec<Posted>` | Exact reversals of the source's moves not reversed yet; a valuation difference goes to the stock adjustment account | `Error::Invalid` when the company has no stock adjustment account and a difference has to go to it; what posting the moves refuses | Reverses once |
| `stock::valuation_rate(tx, tenant, company, item, warehouse) -> Decimal` | An item's current rate in a warehouse, else in any of the company's | | |
| `stock::parse_serials(Option<&str>) -> Vec<String>` | Serial numbers from text, one per line or comma | | |
| `pricing::calculate(k, ctx, tx, &EffEntity, &mut Doc, Side) -> Company` | Fills a priced document's prices, discounts, item tax rates, taxes and totals, after the installed countries choose its taxes. On a person's selling save it drops a typed `price_list_rate` on a new line with an item (or one whose item changed) only when the line entity declares that field with a `read_only_if`; a plainly editable price (a cashier's override) is kept, a plainly read-only one is the kernel's to drop, and elevated writes are untouched | Field errors: no company, no posting date, what the countries refuse | The same document, the same figures |
| `pricing::calc_input`, `tax_in`, `line_tax_rates`, `tax_breakdown`, `breakup` | The calculation's parts over a document's lines, without reading anything | | |
| `localization::check(hc, doc, Side)` | Each installed country's checks before a priced document is submitted: call it in `before_submit` | What the countries refuse | Reads only |
| `flow::sync_invoice_outstanding(k, ctx, tx, entity, invoice)` | Mirrors an open item's outstanding amount and status onto its `SalesInvoice`, `PurchaseInvoice` or `OpeningInvoice`. `allocate` and `unallocate` call it themselves: an app that settles items does not | | Sets the same values |
| `portal::customers_of(k, ctx, tx, email) -> Vec<Uuid>` | The customers an address belongs to: those with it as their email, and those of contacts with it (where the workspace's contacts link a customer), oldest first, at most 50 of each; never a disabled one, nor a guest a web shop records unchecked (anyone may type anyone's address at a guest checkout). Read as the system | | Reads only |
| `portal::suppliers_of(k, ctx, tx, email) -> Vec<Uuid>` | The suppliers an address belongs to, found the same way | | Reads only |
| `portal::customer_for(k, ctx, tx, name, email: Option<&str>, phone: Option<&str>) -> (Uuid, bool)` | A person's customer, found or made, for an app that bills a person: the oldest whose own email is `email` (without one, whose own phone has `phone`'s digits, 6 to 20 of them), never a disabled one nor a web shop's guest, and never through a contact's email (that names someone acting for a customer, often their employer); else an Individual customer made with `ctx`'s rights, holding the name, email and phone. Serialized per workspace and address by a lock held to the end of the caller's transaction, so two calls at once make one customer; with neither an email nor a phone it always makes one. Answers whether it made it now | What saving a Customer refuses: a blank name, a malformed email or phone, `ctx` without the right to create one | Finds the one it made |
| `ledger::round_as(v, precision, rounding) -> Decimal` | `v` at `precision` places, rounded as the company rounds (half up or half to even); every conversion into the company's currency rounds so | | Same figure |
| `localization::ship_to(k, ctx, tx, doc) -> Option<Doc>` | Where a document's goods go, as the countries' rules read it: its ship-to address; else its delivery country and region as an address with only `country_code` and `state`; else the party's address on it | | Reads only |
| `localization::add(Arc<dyn Localization>)` | Registers a country's rules for the workspaces that have its package | | One entry per package: it replaces the earlier |
| `promotion::count_uses(hc, doc, step) -> ()` | Counts each promotion code a submitted document used and each promotion it applied once (`step` 1), or gives them back on a cancel (`step` -1); a return counts nothing, and promotions an app marked `counted_elsewhere` are left out | A code or promotion used up meanwhile, as a field error on `promotion_codes`: of two documents taking the last use at once, the first to commit has it | Counts twice |
| `pricing::snapshot(k, ctx, tx, sale, lines, from, until) -> Snapshot` | What a till keeps to price baskets offline for the days `from` to `until`: the company's currency, tax rows and rounding from `sale`, the prices, pricing rules without a condition and promotions without a code or a first order that could apply on those days, for the items of `lines`; ordered by what it holds, so the same records are the same snapshot | A `sale` without a company | Reads only |

### Stored value

The kind's contracted capabilities `accounting.stored_value_kind.<prove|balance|issue|redeem|refund|transfer|hold|release|reverse>`
are for apps and configuration. The functions from `balance` to `release` run the same operations as the caller, in the
caller's transaction, through the platform's gate: a change happens once per effect `key`, its receipt commits with it,
the same key with other input is refused, and an answer to a key used before has `repeated`. A code and a PIN go only to
`prove`, `balance` and `typed`: a capability that changes something takes the proof `prove` answers (random, for one
operation on one kind, used once, gone after five minutes), never a code; the functions take the code and make the
proof under the key themselves. Every change locks the account first, so of two tills taking from one card the
second waits and sees what the first left, and the database refuses an entry that would take an account below nothing.
Wrong guesses are limited: after five wrong PINs on one account in fifteen minutes, or twenty unknown codes or wrong
PINs by one person, further tries are refused for that time.

| Operation | What it does | What it refuses | Called twice |
|---|---|---|---|
| `stored_value::prove(ctx, tx, kind, operation, key, code, pin) -> (proof, Option<code>)` | Keeps the hashes of a code (a new one is made, and answered once, when none is given) and PIN for one operation; checks nothing against an account | An operation other than `issue`, `redeem`, `refund`, `transfer`, `transfer_to`, `new_code` or `hold`; a code of fewer than eight letters or digits | Under the same key and code, the same proof; another code under the key replaces it |
| `stored_value::balance(k, ctx, tx, kind, code, pin) -> Value` | `{account, balance, holds, state, expires_on}` of the account the code names | An unknown code, a wrong or missing PIN | Reads; each wrong guess counts toward the limit |
| `stored_value::issue(k, ctx, tx, kind, amount, reason, holder, code, pin, key) -> Value` | Value issued by hand, money booked against the kind's Adjustment Account, units carrying no value until used: `{account, entry, code (when one was made), balance}` | A caller the kind's issue role does not allow, a blank reason, an amount outside the kind's loads or its account limit, money in a kind with no Adjustment Account, a disabled kind, a code that is another account's | The first answer, `repeated` |
| `stored_value::redeem(k, ctx, tx, kind, code, pin, invoice, amount, units, key) -> Value` | Pays a sales invoice from the account the code names with a submitted payment entry: `{payment, entry, amount, balance}` | A blocked or expired account, more than the balance, a part where the kind is used whole, an invoice with nothing left to pay, another customer's card | The first answer, `repeated` |
| `stored_value::refund(k, ctx, tx, kind, credit_note, code, pin, amount, key) -> Value` | Puts a credit note's credit onto the account the code names, onto a new card, or onto new store credit held by the customer: `{payment, account, entry, code, balance}` | A credit note with nothing left to give back, a kind used with a PIN when a new card's PIN is missing | The first answer, `repeated` |
| `stored_value::transfer(k, ctx, tx, kind, code, pin, to_code, holder, amount, key) -> Value` | Moves value to another account of the kind or to a new one: `{account, to_account, entry, code, balance}` | A kind that cannot be transferred, the same account, more than the balance, a part where the kind is used whole, an account over the kind's limit | The first answer, `repeated` |
| `stored_value::reverse(k, ctx, tx, kind, entry, key) -> Value` | Undoes what an `entry` from issue, redeem, refund or transfer did (a payment it made is cancelled): `{reversed}` | An unknown entry, a reversal, an expiry, value loaded by a sale (it goes back when the sale is cancelled), an entry already reversed | The first answer, `repeated` |
| `stored_value::hold(k, ctx, tx, kind, code, pin, order, amount, key) -> Value` | Holds an amount on the account for a submitted sales order not yet invoiced: `{hold, account, amount, held, available, expires_at}` | An amount of nothing or less, an order that is not submitted and open, another company's order, another customer's card, a kind that holds units, more than is available | The first answer, `repeated` |
| `stored_value::release(k, ctx, tx, kind, hold, key) -> Value` | Releases a hold: `{hold, state, available}`; a hold already ended is left as it is | A hold that does not exist or is on another kind's account | The first answer, `repeated` |
| `stored_value::typed(k, ctx, tx, code, pin) -> Typed` | Takes a card's code (and PIN) typed in another app for a sale line or refund it hands ERP later: the account the code names, to top up, or the code kept for a new card as a one-time handle (`handle:hint`) the app keeps in place of the code | A code of fewer than eight letters or digits | A new card's code is kept again under another handle |
| `stored_value::forget(ctx, tx, kept)` | Lets a code kept by `typed` go when the app's draft no longer holds it | | Nothing the second time |

Not in it: closing periods, reports, set-up, the trade documents' own hooks and everything under
`addons/`. They are ERP's, and apps reach them through documents and reports.
