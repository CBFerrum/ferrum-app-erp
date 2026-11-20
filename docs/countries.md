# A country described as data

Countries whose VAT or GST works the common way share one engine (`addons/countries/src/engine.rs`). A new one is
three files and no Rust:

- `addons/countries/data/<code>.json`: the description below (the file's stem, two lower-case letters, is the
  package and the module);
- `addons/countries/apps/<code>.yaml`: the app's manifest (copy another country's);
- `addons/countries/packages/<code>/package.yaml`: its fields (registration number on Company, Customer and
  Supplier; the item's category and kind; any company check the description names) and its return definition.

`build.rs` writes a module for each data file with the same entry points a coded country has, and
`ferrum_countries::all()` lists it. The platform compiles each country in by name (`server/Cargo.toml` and
`server/src/packages.rs`): it must list a new module when it pins the release that adds it.

**Statutory content is never written from memory.** Every rate, its first day, every reason and every number rule
carries the official source it was read from on the day, in the file's `sources` (and a rate's own `source`);
what could not be confirmed is left out and said in `sources.not_confirmed`.

## The description
| Key | Meaning |
|---|---|
| `country` | ISO 3166 code of the companies it serves |
| `rows` | the tax rows: `{label, sales, purchases, self_assessed}`; accounts are standard numbers (roles, docs/charts.md); `purchases` null: not recoverable; `self_assessed`: the account debited when the company pays it itself |
| `rates` | `{category, row, from, rate}`: a category's rate on a row from a day (`YYYY-MM-DD`), a decimal percentage |
| `category_field`, `default_category`, `kind_field` | the item's fields; the default category must have a rate (or be `Out of Scope`) |
| `number_field`, `number`, `number_hint` | the registration number's field, its rule (below) and what a valid one looks like |
| `export_reason`, `exempt_reason` | what exports and exempt supplies say on the invoice |
| `reverse_charge` | `"never"`, `"always"` or `{"when": "<company check field>"}` for services bought from abroad |
| `simplified` | `null` or `{field, category}`: a simplified method some companies take |
| `accounts` | `{number, name, parent, root}`: accounts a new company of the country gets (`parent` by the standard chart's name) |
| `sources` | where each fact was read, and on which day |

A file with an unknown key, a rate on a row that does not exist, a `from` that is not a date, a rate that is not a
decimal, or a default category without a rate is refused when read, naming the key (and fails the crate's tests).

## The registration number
`{"strip", "drop", "prefix", "shapes", "checks"}`: the characters in `strip` are removed and the rest upper-cased; a
suffix in `drop` is cut; a `prefix` is taken off (and given back in the normal form); the rest must fit one of
`shapes` (`9` a digit, `A` a letter, `X` either, any other character itself). Each check is
`{"weights", "from": "left" | "right", "modulus", "result": "remainder" | "complement", "fold", "map", "at"}`: the
characters other than the one at `at` (negative counts from the end), read from the left or the right, times the
weights, summed (each product's digits summed when `fold`), modulo `modulus`, as the remainder or `modulus` minus it;
mapped through `map` (`{"11": "0", "10": "K"}`) or a digit; it must equal the character at `at`.

Three worked checks (the engine's unit tests hold them):
- weights `[5,4,3,2,7,6,5,4,3,2]`, left, modulus 11, complement, `{"11": "0"}`, at −1: `20-12345678-?` sums 148,
  148 mod 11 = 5, 11 − 5 = **6**.
- weights `[2,3,4,5,6,7,2,3]`, right, modulus 11, complement, `{"11": "0", "10": "K"}`, at −1, shapes
  `["99999999X", "9999999X"]`: `12.345.678-?` sums 138, 138 mod 11 = 6, **5**; `6.000.000-?` sums 12, 1, 10 → **K**.
- weights `[1,2,1,2,1,2,1,2]`, left, fold, modulus 10, complement, `{"10": "0"}`, at −1: `1234 5678 ?` sums 38, **2**.

A number whose check the source does not describe is checked for its shape only, and its hint says so.

## South Korea
`data/kr.json`: VAT at 10% (Value-Added Tax Act article 30), exports at 0% (article 21), exempt supplies (article 26),
VAT paid by proxy on services from abroad by a company that ticks "Pays VAT on services bought from abroad" (article 52),
booked to the expense account "VAT Paid by Proxy" because it cannot be deducted. The business registration number is
checked for its ten digits only. The return definition "South Korea VAT Return" is ERP's sums, not the National Tax
Service form's numbering.
