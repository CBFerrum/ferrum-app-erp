# Carrier connections

A **Carrier Connection** (Inventory menu) describes a carrier, or a service that speaks for many, as four
HTTP calls. ERP knows no carrier: what to send and where to find the answer is the connection's
`description` (Calls), a JSON object:

| Part | What it is |
| --- | --- |
| `rate` | asks for rates: `rates_at` (the list), and in each rate `service_at`, `amount_at`, `currency_at`, `days_at`, `rate_id_at` |
| `label` | buys the label: `tracking_at`, `label_at` (the file in base64) or `label_url_at` (where to fetch it), `label_type` (`pdf`, `zpl`, `png`), `tracking_url_at`, `cost_at` (else the rate), `id_at` (the carrier's id for the label) |
| `track` | where the parcels are: `status_at`, `statuses` (the carrier's words to In Transit, Delivered or Exception) |
| `cancel` | cancels the label |
| `tracking_link` | a template giving the link people follow, when the label's answer has none |
| `inbound` | how the carrier's own tracking calls are checked and read: `auth` `hmac` (HMAC-SHA256 of the body under the Inbound Secret, hex, in `header`, `sha256=` before it or not) or `token` (the secret in `header` or the `token` query parameter), `tracking_at`, `status_at`, `statuses` |

Each call has `url`, `method` (POST), `headers`, and a `body` (a JSON template) or a `form`, all templates
(MiniJinja, `| json` writes a value as JSON) over: `shipment` (the record, its `packages` with `weight`,
`length`, `width`, `height`, `reference`, `contents`, and its `reference`), `to` and `from` (the ship-to
address and the connection's Ship From, as Address records: `address_line1`, `city`, `state`, `postal_code`,
`country_code`, `phone`, `email`), `rate` (`id`, `service`, `amount`, `currency`), `tracking_number`,
`provider_id`, `weight_unit`, `dimension_unit`, `total_weight`, `secret.api_key`, `secret.api_secret`, and
`key`: the shipment's id, the same on every attempt, to send as the carrier's idempotency key. `error_at`
says where the answer explains a refusal; `temporary` lists the statuses meaning "try again later" (408,
425, 429, 500, 502, 503, 504 when left out). Pointers are JSON pointers (`/a/0/b`); the rate's are
relative to each rate.

The API Key, API Secret and Inbound Secret are secret fields: sealed with the server's key, never read
back. A **Test Carrier** answers each call from its `fixtures` (`{rate, label, track, cancel}`, each the
answer the call would get) and calls nobody.

The carrier's tracking calls go to `POST /api/site/{workspace}/inventory/carriers/{connection id}`; one
not signed as `inbound` says is refused (401) and counted on the connection (Refused Inbound Calls). The
upkeep tracks labelled shipments every *Track Every (Hours)* (6; 0: only when asked or told).

**On a delivery note or a purchase return**: Ship by Carrier makes a Carrier Shipment with the first
parcel; add parcels; Get Rates; Choose Rate; Buy Label (the file attached to the shipment, the tracking
number and link kept; pressed twice, or retried, it buys once); Track; Cancel Label. *Label Cost* on the
connection: Not Booked; Absorbed (the cost is booked to the Freight Account against the Freight Owed
Account until the carrier's bill comes, taken back when the label is cancelled); Charged to Customer (the
same, and the invoice made from the delivery charges it once on a line of the Freight Item).

## Worked example: one API over many carriers

The addresses are placeholders.

```json
{
  "rate": {
    "url": "https://api.multicarrier.example/v2/shipments",
    "headers": {"Authorization": "Bearer {{ secret.api_key }}"},
    "body": "{\"to_address\": {\"name\": {{ to.address_title|json }}, \"street1\": {{ to.address_line1|json }}, \"city\": {{ to.city|json }}, \"zip\": {{ to.postal_code|json }}, \"country\": {{ to.country_code|json }}}, \"from_address\": {\"street1\": {{ from.address_line1|json }}, \"city\": {{ from.city|json }}, \"zip\": {{ from.postal_code|json }}, \"country\": {{ from.country_code|json }}}, \"parcels\": [{% for p in shipment.packages %}{\"weight\": {{ p.weight|json }}, \"length\": {{ p.length|json }}, \"width\": {{ p.width|json }}, \"height\": {{ p.height|json }}, \"mass_unit\": {{ weight_unit|json }}, \"distance_unit\": {{ dimension_unit|json }}}{% if not loop.last %},{% endif %}{% endfor %}], \"reference\": {{ shipment.reference|json }}}",
    "rates_at": "/rates", "service_at": "/service", "amount_at": "/amount", "currency_at": "/currency", "days_at": "/estimated_days", "rate_id_at": "/id",
    "error_at": "/error/message"
  },
  "label": {
    "url": "https://api.multicarrier.example/v2/transactions",
    "headers": {"Authorization": "Bearer {{ secret.api_key }}", "Idempotency-Key": "{{ key }}"},
    "body": "{\"rate\": {{ rate.id|json }}, \"label_file_type\": \"PDF\"}",
    "tracking_at": "/tracking_number", "label_url_at": "/label_url", "tracking_url_at": "/tracking_url_provider", "id_at": "/object_id", "cost_at": "/rate/amount",
    "error_at": "/messages/0/text"
  },
  "track": {"url": "https://api.multicarrier.example/v2/tracks/{{ tracking_number }}", "method": "GET", "headers": {"Authorization": "Bearer {{ secret.api_key }}"},
            "status_at": "/tracking_status/status", "statuses": {"TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILURE": "Exception", "RETURNED": "Exception"}},
  "cancel": {"url": "https://api.multicarrier.example/v2/refunds", "headers": {"Authorization": "Bearer {{ secret.api_key }}"}, "body": "{\"transaction\": {{ provider_id|json }}}"},
  "inbound": {"auth": "hmac", "header": "X-Signature", "tracking_at": "/data/tracking_number", "status_at": "/data/tracking_status/status",
              "statuses": {"TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILURE": "Exception"}}
}
```

## Worked example: a carrier's own API, with a form and a thermal label

```json
{
  "rate": {
    "url": "https://api.parcelcarrier.example/rating",
    "form": {"account": "{{ secret.api_secret }}", "postcode": "{{ to.postal_code }}", "country": "{{ to.country_code }}", "weight": "{{ total_weight }}", "pieces": "{{ shipment.packages|length }}"},
    "headers": {"X-Api-Key": "{{ secret.api_key }}"},
    "rates_at": "/products", "service_at": "/code", "amount_at": "/charge/total", "currency_at": "/charge/currency", "days_at": "/transit",
    "temporary": [429, 503], "error_at": "/fault/detail"
  },
  "label": {
    "url": "https://api.parcelcarrier.example/consignments",
    "headers": {"X-Api-Key": "{{ secret.api_key }}", "X-Request-Id": "{{ key }}"},
    "body": "{\"product\": {{ rate.service|json }}, \"receiver\": {\"line1\": {{ to.address_line1|json }}, \"town\": {{ to.city|json }}, \"postcode\": {{ to.postal_code|json }}, \"phone\": {{ to.phone|json }}}, \"pieces\": [{% for p in shipment.packages %}{\"kg\": {{ p.weight|json }}, \"ref\": {{ p.reference|json }}}{% if not loop.last %},{% endif %}{% endfor %}], \"format\": \"ZPL\"}",
    "tracking_at": "/consignment/number", "label_at": "/consignment/labels/0/data", "label_type": "zpl", "id_at": "/consignment/id",
    "error_at": "/fault/detail"
  },
  "track": {"url": "https://api.parcelcarrier.example/consignments/{{ provider_id }}/events", "method": "GET", "headers": {"X-Api-Key": "{{ secret.api_key }}"},
            "status_at": "/events/0/code", "statuses": {"OUT": "In Transit", "HUB": "In Transit", "POD": "Delivered", "EXC": "Exception"}},
  "cancel": {"url": "https://api.parcelcarrier.example/consignments/{{ provider_id }}", "method": "DELETE", "headers": {"X-Api-Key": "{{ secret.api_key }}"}},
  "tracking_link": "https://track.parcelcarrier.example/?c={{ tracking_number }}",
  "inbound": {"auth": "token", "header": "X-Hook-Token", "tracking_at": "/consignment", "status_at": "/code",
              "statuses": {"POD": "Delivered", "EXC": "Exception", "OUT": "In Transit"}}
}
```

## What the platform would need for this copy to go

ERP runs these calls itself (`src/shipping.rs`), modelled on the platform's message channels, because the
platform describes calls only for text and chat channels. The platform would need to offer, for any app: a
**described call on a connection** (a stored connection with sealed secrets; a call rendered from templates
over values the app passes; sent through the vetted client; the answer read by pointers, with `error_at` and
temporary statuses; an idempotency key per attempt series), and **signed inbound calls per connection** (an
address per connection, `hmac` or `token`, refusals counted, the body handed to the app).
