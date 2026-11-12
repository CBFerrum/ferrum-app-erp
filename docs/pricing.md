# Pricing a basket from data alone

`ferrum-erp-pricing` (`pricing/`) prices a whole basket from a **snapshot** of the records a sale is priced with:
each line's price, its pricing rule, the promotions over the whole basket, the taxes, the document's discount, the
rounding, the totals and the tax summary. It has no database and no clock (the moment is an input), uses exact
decimals only and names no country; the same snapshot and basket give the same bytes out.

ERP's server prices its documents with the same pieces (`list_price`, `unit_factor`, `best_rule`,
`line_tax_rates`, `before_promotions`, `tax_summary`, then `ferrum-calc` and `ferrum-erp-promotions`), and
`price_basket` puts them together in the server's order. A till keeps a snapshot and runs the crate built for the
browser; the server prices a sale the till made offline again with the snapshot the till named.

Why a crate of its own beside `ferrum-erp-promotions`: promotions stay one engine with one job, which the server,
other apps and this crate call; pricing a basket needs promotions *and* prices, rules, units and the tax summary, and
it is what the browser loads (one WebAssembly file with `ferrum-calc` and the promotions inside).

## Schema 1

Every field below is JSON. Money, quantities, rates and percents are **decimal strings** (`"12.50"`), never numbers.
A reader refuses a `schema` it does not know.

### Snapshot

| Field | | |
|---|---|---|
| `schema` | `1` | |
| `valid_from`, `valid_until` | `YYYY-MM-DD` | The days it prices for, both included, in `time_zone`. A basket at another moment is refused. |
| `time_zone` | text or null | The company's; the moment of a basket is in it. |
| `currency` | text | The sale's currency code. |
| `precision` | number | Its decimal places. |
| `base_precision` | number or null | The company currency's. |
| `conversion_rate` | decimal | Sale currency to company currency (`"1"` when the same). Prices from the price list are divided by it. |
| `rounding` | `half_up` / `half_even` | How amounts round. |
| `round_total_to` | decimal or null | Cash rounding of the total (`"0.05"`). |
| `tax_rounding` | `per_document` / `per_line` | |
| `taxes` | list | The tax rows, in order: `key`, `charge_type` (`on_net_total`, `on_previous_row_amount`, `on_previous_row_total`, `actual`, `on_item_quantity`), `rate`, `amount`, `row_ref` (an earlier row's place, from 0), `included_in_price`, `deduct`, `reverse_charge`, and for the summary `description`, `account`, `category`, `exemption_reason`. |
| `items` | list | `id`, `item_group`, `stock_uom`, `units` (`[{uom, factor}]`: stock units in one), `standard_rate` (per stock unit, when the price list has no price), `delivery` (a delivery charge), `tax_rates` (`[{tax_account, rate, category, exemption_reason}]`: the item's own rates for the tax rows of that account, as the countries' rules leave them). |
| `prices` | list | The price list's prices: `item`, `uom` (null: the stock unit), `rate`, `valid_from`, `valid_upto`. |
| `rules` | list | Pricing rules without a condition (one with a condition is the server's alone): `id`, `apply_on` (`Item`, `Item Group`, else any), `item`, `item_group`, `customer`, `customer_group`, `min_qty`, `max_qty` (stock units; `"0"` no most), `valid_from`, `valid_upto`, `priority` (null: 1), `discount_percent`, `rate` (per stock unit). |
| `promotions` | list | In `ferrum-erp-promotions`' form (`Promotion`), amounts in the sale's currency; none needing a code or a first order. |
| `customer`, `customer_group` | text or null | Who buys when the basket names nobody (the register's walk-in customer). |
| `channel`, `place` | text or null | What promotions call where the sale is made. |

### Basket

| Field | | |
|---|---|---|
| `lines` | list | `item`, `qty`, `uom` (null: the stock unit), `price` (typed; null: the price list's), `discount_percent` (typed; null: the pricing rule's). |
| `at` | `YYYY-MM-DD HH:MM` | The moment, in the snapshot's time zone. |
| `customer`, `customer_group` | text or null | Null: the snapshot's. |
| `discount_percent` | decimal | The document's own discount. |

### Priced

| Field | | |
|---|---|---|
| `schema` | `1` | |
| `lines` | list | The basket's lines, then the free items promotions added: `key` (the line's place, `"0"`, `"1"`, …), `item`, `qty`, `uom`, `conversion_factor`, `stock_qty`, `price_list_rate`, `discount_percent` (its own), `promotion_percent` (the exact share promotions took of what its own discount left), `added_by_promotion`, `tax_rates` (by tax row key). |
| `applied` | list | Each promotion's part of each line: `promotion`, `title`, `line` (a line's `key`), `amount`, `code`. |
| `totals` | object | `ferrum-calc`'s output: each line's `rate`, `amount`, `net_amount`, `taxes`; each tax row's `tax_amount`, `total`; `total`, `discount_amount`, `net_total`, `total_taxes`, `grand_total`, `rounding_adjustment`, `rounded_total`, and the base currency's. |
| `tax_summary` | list | Per tax row, category, rate and exemption reason: `tax` (the row's place), `description`, `account`, `category`, `rate`, `exemption_reason`, `reverse_charge`, `taxable_amount`, `tax_amount`, `base_taxable_amount`, `base_tax_amount`. |

An error comes back as `{"error": "..."}`.

### How a line is priced (the server's order)

1. Its unit: the stock unit, else one of the item's units (refused when the item has no conversion for it).
2. Its price: the typed one; else the price list's latest price in force that day for that unit, else for the stock
   unit times the unit's factor; else the item's standard rate times the factor; divided by `conversion_rate`.
3. Its pricing rule, when no discount was typed: the rules that fit the item, customer, stock quantity and day; the
   highest priority, then the largest discount, then the last. A rule's rate (times the factor) replaces the price,
   a rule's discount becomes the line's.
4. Promotions, on what each line comes to with its own discount (no taxes, no document discount, no cash rounding).
   A free item a promotion adds is a line priced like any other, and the basket is priced again.
5. Each line priced with its own discount, then the promotions' share of what that leaves; taxes, the document's
   discount, cash rounding; the summary.

## The browser build

`scripts/pricing-wasm.sh` builds `priceBasket(json) -> json` with wasm-bindgen 0.2.129, the version Ferrum-Frontend
pins (the workspace's `Cargo.toml` pins it too, so one `wasm-bindgen-cli` builds `@ferrum/calc` and this), and
prints the files' SHA-256 digests. With `--check` it prices `pricing/tests/baskets.jsonl` through the WebAssembly in
Node and compares every answer byte for byte with `pricing/tests/priced.jsonl`, which the native test
`fixtures_are_priced_byte_for_byte` holds the crate to (`UPDATE_PRICING_FIXTURES=1` writes both again).
