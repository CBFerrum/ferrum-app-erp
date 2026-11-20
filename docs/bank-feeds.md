# Bank feeds

A bank feed brings a bank account's transactions without a file. Finance names no bank or aggregator: an
administrator describes the provider's API once as a **connection** (Administration, Connections; the platform's
`docs/api.md`, *Connections*) whose **pull** asks for new transactions every few hours and writes each one as a
**Bank Feed Line**. Finance turns each feed line into a submitted bank transaction of the bank account it belongs
to, past the same duplicate check as a statement file, and applies the automatic bank rules.

## Linking a bank account

On the bank account, **Feed Connection** is the connection's name and **Feed Account Reference** the provider's id of
the account, as its transactions carry it. Both or neither; no two bank accounts share a pair. A feed line for an
account not linked yet waits (state Waiting); once the account is linked, **Take Feed Lines** on it brings up to 500
waiting lines at a time (`finance.bank_account.take_feed_lines`).

## A worked connection

A placeholder provider, `api.bankfeed.example`, answers `GET /v1/transactions?from=…` with
`{"transactions": [{"id", "account_id", "booking_date", "amount", "remittance", "end_to_end_id"}]}`, money out a
negative amount:

```json
{
  "base": "https://api.bankfeed.example/v1",
  "auth": {"type": "bearer", "secret": "api_key"},
  "operations": {
    "transactions": {"path": "/transactions", "inputs": ["from"], "query": {"from": "{{ inputs.from }}"}}
  },
  "pulls": [{
    "name": "transactions", "operation": "transactions", "every_minutes": 240,
    "items_at": "/transactions", "cursor": "time", "cursor_input": "from",
    "map": {
      "entity": "BankFeedLine",
      "by": {"connection": "connection.name", "external_id": "/id"},
      "create": {"account_ref": "/account_id", "date": "/booking_date", "amount": "/amount",
                 "description": "/remittance", "reference": "/end_to_end_id"},
      "as": "system", "outside": true
    }
  }]
}
```

`cursor: time` gives each run the moment the last one began, as the `from` input. In the mapping a value starting
with `/` is a pointer into the item, any other an expression (`connection.name` is the connection's name). `by` finds
the feed line again, so a page pulled twice changes nothing. `as: system` lets the mapping set the feed line's
fields, which people cannot write; only an administrator saves such a description. The API key is the connection's
secret, kept in the vault; Finance never reads it.

## What a provider must offer

- **A stable id for each transaction.** It is the feed line's `external_id` and the bank transaction's bank
  reference: the same id is the same transaction. A provider whose ids change between calls must not be described
  this way.
- **A signed amount**, or a field to derive the sign from in the mapping (an expression such as
  `item.direction == 'DEBIT' ? -item.amount : item.amount`). A zero amount is refused for that item.
- A booking date (ISO), and paging the platform's pull can follow (`next_at`) when a run may return many.

## Guarantees and ceilings

- A provider's transaction is taken once: unique connection and id on the feed line, and unique bank account and
  hash on the bank transaction.
- A file and a feed do not double a line when the file carries the same bank reference (camt's `AcctSvcrRef`, OFX's
  `FITID`) as the feed's id. A CSV without the bank's references is not recognised as the same line.
- A feed line becomes a transaction in the transaction that saved it: a failure leaves neither, and the pull tries
  the item again at its next run.
- Pending transactions and balances are not read. No provider is described in this repository.
