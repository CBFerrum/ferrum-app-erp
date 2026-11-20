# Statement formats

A **Statement Format** (Finance menu) says how a bank's statement file is read: which column, element or
tag holds each line's date, amount, text and references. Finance knows no standard: it has three readers,
and what a standard looks like is a format's `reading`, a JSON object. Four formats ship as records (below);
an Accounts Manager adds one for a bank's own CSV, or for a bank whose variant of a standard differs, without
a release. An Accounts User reads them.

| Kind | What it reads |
| --- | --- |
| Delimited | Rows of columns (CSV), a header row naming them |
| Markup | Elements: XML, or SGML whose leaf tags are not closed (`loose`) |
| Tagged | Lines that start with `:TAG:` (1 to 3 letters or digits); a line that does not continues the field above |

## Choosing the format

An import (`preview_statement`, `import_transactions` on the bank account, both `{text, format?, inline?}`)
reads the text with, in this order: the `format` named; `inline`, a Delimited `reading` as JSON (the columns
chosen on the import screen); the first enabled format, by name, whose `sniff` fits the text; the bank
account's own Statement Format. When none fits, the preview answers `{"format": null}` and the import is
refused ("choose how this statement is read"). A disabled format named is refused.

## The reading

An unknown key is refused when a format is saved. A path is element names joined by `/`, without namespaces,
relative to the entry (or to the statement, for `account`); no wildcards, no attributes.

| Key | Delimited | Markup | Tagged |
| --- | --- | --- | --- |
| `sniff` | – | list of strings; the format fits a file whose first 4,096 characters contain one | same |
| `delimiter`, `header_row` | one character (found from the header when left out); the header's line, 1 when left out | – | – |
| `columns` | `date`, `amount` or `deposit` and `withdrawal`, `description`, `reference`, `bank_reference`: each a header (any case) or a 1-based column number. One `amount` column is signed, money out a minus | – | – |
| `loose` | – | `true`: leaf tags are closed before reading, and what comes before the first `<` is dropped | – |
| `statement` | – | list of element names; each occurrence is one account's statement. Empty: the whole file is one | – |
| `account` | – | list of paths, inside a statement, to the account's number | `{tag}`: the field holding it |
| `entry` | – | the element name of a line; each occurrence inside a statement is one | – |
| `line` | – | – | `{tag, pattern}`: the field that is a line, and a regular expression with named groups `date`, `sign`, `amount`, and as needed `reference`, `bank_reference`, `description` (at most 500 characters) |
| `detail` | – | – | `{tag, strip?}`: a field right after a line whose text is the line's description; each match of `strip` (a regular expression) becomes a space |
| `date`, `amount`, `description`, `reference`, `bank_reference` | – | each a list of paths inside the entry. The first value found is taken; `description` joins every value found, each once, with a space | – |
| `counterparty` | – | `{in: [paths], out: [paths]}`: the other party's name for money in and for money out, put before the description unless it is in it | – |
| `sign` | – | `{at: [paths], in: [values], out: [values]}`; without it the amount is signed | `{in: [values], out: [values]}` for the `sign` group |
| `only` | – | `{at: [paths], is: [values]}`: an entry with another value there is left out; one without the element is kept | – |
| `empty` | – | values read as nothing (`NOTPROVIDED`) | same (`NONREF`) |
| `date_format`, `date_chars`, `decimal_comma` | `date_format`, `decimal_comma` | all three: `date_chars` keeps the first n characters of the date before reading it | all three |

A date is read with `date_format` (chrono's syntax, as `%y%m%d`), or as ISO, day first with `/`, `.` or `-`,
or `12 Mar 2026`. An amount ignores currency signs, spaces and thousands separators; `decimal_comma` reads
`1.234,56`. A line with no amount, and a Markup entry `only` leaves out, is counted as left out.

## What a line is

A line that carries the bank's own reference (`bank_reference`: camt's `AcctSvcrRef`, MT940's reference for
the bank, OFX's `FITID`) is that reference on that account: the same reference again, from this file or any
other, in any format, is the same line, and two lines of one file with one reference are one line. A line
without it is its date, amount, description and reference; identical lines of one file are numbered, so two
equal payments on one day are both kept. Every line already on the account is left out, so importing a file
again, or one that overlaps, adds only what is new; a cancelled transaction no longer counts. The unique key
`[bank_account, transaction_hash]` keeps one line once even when two imports race: the second fails on it and
saves nothing.

**Known ceiling.** A line first imported from a file without the bank's reference (a CSV with no such column)
and later from one that carries it is not recognised as the same line: its two identities differ.

## Another account's statement

When the file names the account (Markup `account`, Tagged `account`) and the bank account has an IBAN or an
account number, statements of another account are left out (`other_accounts` counts them). Both numbers are
compared as letters and digits in capitals: equal, or the longer ends with the shorter when that has at least 6
characters. A file only of another account's statements is refused, naming that account masked
(`•••• 5432`).

## Limits

The text is at most 5 MB and 10,000 lines. A Markup file that is not well formed or nests deeper than 64
levels is one error; no DTD is read and an entity other than the five XML ones and character references reads
as nothing. Regular expressions run in linear time, compiled to at most 1 MB. A description keeps its first
1,000 characters, a reference and the bank's reference their first 140. The preview lists the first 50 lines
it cannot read; the import refuses the whole file for any of them.

## The formats Finance ships

| Format | Kind | Reads |
| --- | --- | --- |
| ISO 20022 statement (camt.053) | Markup | Booked entries (`Sts` or `Sts/Cd` is `BOOK`) of each `Stmt`, account by `Acct/Id/IBAN` or `Acct/Id/Othr/Id`; sign by `CdtDbtInd`; date `BookgDt` then `ValDt`; text from `Ustrd` and `AddtlNtryInf` with the debtor's or creditor's name; reference `EndToEndId` or the structured creditor reference; the bank's `AcctSvcrRef` or `NtryRef`. A batch entry is one line |
| ISO 20022 account report (camt.052) | Markup | The same, for each `Rpt` |
| SWIFT MT940 | Tagged | Field `:61:` (value date, `C`, `D`, `RC`, `RD`, amount with a decimal comma, the reference for the account owner, `//` the bank's reference), its text from the `:86:` after it with `?nn` subfield marks made spaces; the account from `:25:`; statements end at `-` |
| OFX and QFX | Markup, loose | `STMTTRN` of `STMTRS` (bank) or `CCSTMTRS` (card), account `ACCTID`; `DTPOSTED`, signed `TRNAMT`, `NAME` and `MEMO`, `CHECKNUM` or `REFNUM`, `FITID`. OFX 1 (SGML) and 2 (XML) alike |

Each is a record: change it, or disable it and add another, when a bank writes its file otherwise.
