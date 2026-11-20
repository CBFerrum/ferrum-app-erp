# Carrier connections

A **Carrier Connection** (Inventory menu) describes a carrier, or a service that speaks for many, as four
HTTP calls. ERP knows no carrier: what to send and where to find the answer is the carrier's
`description` (Calls), a JSON object:

| Part | What it is |
| --- | --- |
| `rate` | asks for rates: `rates_at` (the list), and in each rate `service_at`, `amount_at`, `currency_at`, `days_at`, `rate_id_at` |
| `label` | buys the label: `tracking_at`, `label_at` (the file in base64) or `label_url_at` (where to fetch it), `label_type` (`pdf`, `zpl`, `png`), `tracking_url_at`, `cost_at` (else the rate), `id_at` (the carrier's id for the label) |
| `track` | where the parcels are: `status_at`, `statuses` (the carrier's words to In Transit, Delivered or Exception) |
| `cancel` | cancels the label |
| `tracking_link` | a template over `tracking_number` and `shipment` giving the link people follow, when the label's answer has none |
| `inbound` | how the carrier's own tracking calls are read: `auth` `token` (the Inbound Secret sent in `header`, `X-Webhook-Token` when not named; the platform checks it, and a description with any other `auth` is refused), `tracking_at`, `status_at`, `statuses` |

Each call has `url`, `method` (POST), `headers`, and a `body` (a JSON template) or a `form`, all templates
of the carrier's connection (MiniJinja, `| json` writes a value as JSON) over its inputs: `inputs.shipment`
(the record, its `packages` with `weight`, `length`, `width`, `height`, `reference`, `contents`, and its
`reference`), `inputs.to` and `inputs.from` (the ship-to address and the connection's Ship From, as Address
records: `address_line1`, `city`, `state`, `postal_code`, `country_code`, `phone`, `email`), `inputs.rate`
(`id`, `service`, `amount`, `currency`), `inputs.tracking_number`, `inputs.provider_id`, `inputs.weight_unit`,
`inputs.dimension_unit`, `inputs.total_weight`; and over `secret.api_key`, `secret.api_secret` and `key`: the
shipment's id, the same on every attempt, to send as the carrier's idempotency key. `error_at` says where the
answer explains a refusal; `temporary` lists the statuses meaning "try again later" (408, 425, 429, 500, 502,
503, 504 when left out). Pointers are JSON pointers (`/a/0/b`); the rate's are relative to each rate.

## Connections

The calls are made by the platform's connections (Administration, Connections). Saving a carrier makes one
connection of the workspace for it, named `carrier_` and eight digits (one more, `…_2`, for each further
address its calls go to), whose operations `rate`, `label`, `track` and `cancel` are the carrier's calls with
each `url` split into the connection's base and the operation's path (a template in the host cannot be split
and is refused). The carrier's *Enabled* turns its connection on; its description is the carrier's, and saving
it rewrites the connection's.

The API Key, API Secret and Inbound Secret typed on a carrier are **moved** into its connection's secrets on
saving and cleared from the carrier, so only the connection holds them (sealed, never read back, never in a
log, an audit row or a job). To change one, type every key again, or change them under Connections.

A **Test Carrier** answers each call from its `fixtures` (`{rate, label, track, cancel}`, each the answer the
call would get) and calls nobody. A label is bought with the shipment's id as the connection's call key: the
carrier is called once even if the first answer was lost with a rolled-back run.

The carrier's tracking calls go to the connection's inbound address, `POST /api/connections/inbound/{id}`
(shown under Connections), with the Inbound Secret in `inbound.header`; a call that is wrong, replayed or
too large is refused alike and counted on the connection. The upkeep tracks labelled shipments every *Track
Every (Hours)* (6; 0: only when asked or told).

A label the answer gives as an address (`label_url_at`) is fetched with the platform's vetted client, for the
shipment's workspace (a sandbox copy fetches nothing): the connection's operations read JSON, not files.

**On a delivery note or a purchase return**: Ship by Carrier makes a Carrier Shipment with the first
parcel; add parcels; Get Rates; Choose Rate; Buy Label (the file attached to the shipment, the tracking
number and link kept; pressed twice, or retried, it buys once); Track; Cancel Label. *Label Cost* on the
connection: Not Booked; Absorbed (the cost is booked to the Freight Account against the Freight Owed
Account until the carrier's bill comes, taken back when the label is cancelled); Charged to Customer (the
same, and the invoice made from the delivery charges it once on a line of the Freight Item).

## Templates and comparing carriers
A **Carrier Template** is the same description as a connection's Calls, with what the connection needs said in words
and how far it was proven (recorded answers, the provider's sandbox, live). A new connection that names a template and
leaves Calls empty takes the template's calls once; edit them freely afterwards. A template never holds a key: type
keys on the connection.

A shipment can hold several carriers' offers: Get Rates with `carrier` asks that carrier (one call, its own request) and
replaces only that carrier's earlier offers; each offer names its `carrier`. Choose Rate with `service` and `carrier`
makes that carrier the shipment's; Buy Label then buys from it. The shipment desk's **Compare carriers** asks every
enabled carrier of the company this way, each on its own.

## Worked example: one API over many carriers

The addresses are placeholders.

```json
{
  "rate": {
    "url": "https://api.multicarrier.example/v2/shipments",
    "headers": {"Authorization": "Bearer {{ secret.api_key }}"},
    "body": "{\"to_address\": {\"name\": {{ inputs.to.address_title|json }}, \"street1\": {{ inputs.to.address_line1|json }}, \"city\": {{ inputs.to.city|json }}, \"zip\": {{ inputs.to.postal_code|json }}, \"country\": {{ inputs.to.country_code|json }}}, \"from_address\": {\"street1\": {{ inputs.from.address_line1|json }}, \"city\": {{ inputs.from.city|json }}, \"zip\": {{ inputs.from.postal_code|json }}, \"country\": {{ inputs.from.country_code|json }}}, \"parcels\": [{% for p in inputs.shipment.packages %}{\"weight\": {{ p.weight|json }}, \"length\": {{ p.length|json }}, \"width\": {{ p.width|json }}, \"height\": {{ p.height|json }}, \"mass_unit\": {{ inputs.weight_unit|json }}, \"distance_unit\": {{ inputs.dimension_unit|json }}}{% if not loop.last %},{% endif %}{% endfor %}], \"reference\": {{ inputs.shipment.reference|json }}}",
    "rates_at": "/rates", "service_at": "/service", "amount_at": "/amount", "currency_at": "/currency", "days_at": "/estimated_days", "rate_id_at": "/id",
    "error_at": "/error/message"
  },
  "label": {
    "url": "https://api.multicarrier.example/v2/transactions",
    "headers": {"Authorization": "Bearer {{ secret.api_key }}", "Idempotency-Key": "{{ key }}"},
    "body": "{\"rate\": {{ inputs.rate.id|json }}, \"label_file_type\": \"PDF\"}",
    "tracking_at": "/tracking_number", "label_url_at": "/label_url", "tracking_url_at": "/tracking_url_provider", "id_at": "/object_id", "cost_at": "/rate/amount",
    "error_at": "/messages/0/text"
  },
  "track": {"url": "https://api.multicarrier.example/v2/tracks/{{ inputs.tracking_number }}", "method": "GET", "headers": {"Authorization": "Bearer {{ secret.api_key }}"},
            "status_at": "/tracking_status/status", "statuses": {"TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILURE": "Exception", "RETURNED": "Exception"}},
  "cancel": {"url": "https://api.multicarrier.example/v2/refunds", "headers": {"Authorization": "Bearer {{ secret.api_key }}"}, "body": "{\"transaction\": {{ inputs.provider_id|json }}}"},
  "inbound": {"auth": "token", "header": "X-Signature", "tracking_at": "/data/tracking_number", "status_at": "/data/tracking_status/status",
              "statuses": {"TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILURE": "Exception"}}
}
```

## Worked example: a carrier's own API, with a form and a thermal label

```json
{
  "rate": {
    "url": "https://api.parcelcarrier.example/rating",
    "form": {"account": "{{ secret.api_secret }}", "postcode": "{{ inputs.to.postal_code }}", "country": "{{ inputs.to.country_code }}", "weight": "{{ inputs.total_weight }}", "pieces": "{{ inputs.shipment.packages|length }}"},
    "headers": {"X-Api-Key": "{{ secret.api_key }}"},
    "rates_at": "/products", "service_at": "/code", "amount_at": "/charge/total", "currency_at": "/charge/currency", "days_at": "/transit",
    "temporary": [429, 503], "error_at": "/fault/detail"
  },
  "label": {
    "url": "https://api.parcelcarrier.example/consignments",
    "headers": {"X-Api-Key": "{{ secret.api_key }}", "X-Request-Id": "{{ key }}"},
    "body": "{\"product\": {{ inputs.rate.service|json }}, \"receiver\": {\"line1\": {{ inputs.to.address_line1|json }}, \"town\": {{ inputs.to.city|json }}, \"postcode\": {{ inputs.to.postal_code|json }}, \"phone\": {{ inputs.to.phone|json }}}, \"pieces\": [{% for p in inputs.shipment.packages %}{\"kg\": {{ p.weight|json }}, \"ref\": {{ p.reference|json }}}{% if not loop.last %},{% endif %}{% endfor %}], \"format\": \"ZPL\"}",
    "tracking_at": "/consignment/number", "label_at": "/consignment/labels/0/data", "label_type": "zpl", "id_at": "/consignment/id",
    "error_at": "/fault/detail"
  },
  "track": {"url": "https://api.parcelcarrier.example/consignments/{{ inputs.provider_id }}/events", "method": "GET", "headers": {"X-Api-Key": "{{ secret.api_key }}"},
            "status_at": "/events/0/code", "statuses": {"OUT": "In Transit", "HUB": "In Transit", "POD": "Delivered", "EXC": "Exception"}},
  "cancel": {"url": "https://api.parcelcarrier.example/consignments/{{ inputs.provider_id }}", "method": "DELETE", "headers": {"X-Api-Key": "{{ secret.api_key }}"}},
  "tracking_link": "https://track.parcelcarrier.example/?c={{ tracking_number }}",
  "inbound": {"auth": "token", "header": "X-Hook-Token", "tracking_at": "/consignment", "status_at": "/code",
              "statuses": {"POD": "Delivered", "EXC": "Exception", "OUT": "In Transit"}}
}
```
