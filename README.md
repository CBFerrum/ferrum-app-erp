<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
  <img alt="Ferrum ERP" src="docs/assets/banner-light.svg" width="100%">
</picture>

[![CI](https://github.com/CBFerrum/ferrum-app-erp/actions/workflows/ci.yml/badge.svg)](https://github.com/CBFerrum/ferrum-app-erp/actions/workflows/ci.yml) ![Version](https://img.shields.io/badge/version-0.4.0-2f5bea?style=flat-square) ![Ferrum](https://img.shields.io/badge/Ferrum-%3E%3D0.3.1%2C%20%3C0.4-2f5bea?style=flat-square) ![License](https://img.shields.io/badge/license-AGPL--3.0--or--later-3b82f6?style=flat-square) ![Rust](https://img.shields.io/badge/Rust-native%20app-b7410e?style=flat-square&logo=rust&logoColor=white) ![Interface](https://img.shields.io/badge/interface-React%20%2B%20Vite-0ea5e9?style=flat-square&logo=react&logoColor=white) ![MCP](https://img.shields.io/badge/AI%20agents-MCP-8b5cf6?style=flat-square)

**A native app for [Ferrum](https://github.com/CBFerrum/Ferrum), the open-source, AI-first business platform.**

[Overview](#overview) · [Modules](#modules) · [Countries](#every-country-as-an-add-on) · [Screenshots](#screenshots) · [How it works](#how-it-works) · [Repository](#this-repository) · [Install](#install) · [Develop](#develop) · [Changelog](#changelog)

</div>

---

## Overview

Ferrum ERP is accounting, selling, buying, stock and manufacturing on one universal journal that
is append-only and balanced in the database. Documents carry exact tax maths, stock moves through
an append-only ledger with moving-average and FIFO cost layers (a backdated entry posts forward;
nothing is ever reposted), and every figure the reports show comes from the same ledger. Its
add-ons bring budgets and bank reconciliation, procurement, stock operations and manufacturing
operations. A country's taxes and rules are an add-on too, on one localization seam: India's (GST,
e-invoices and e-way bills), the European Union's (VAT, VIES, the One-Stop Shop, EN 16931
e-invoices over Peppol), Germany's (XRechnung, small businesses, the domestic reverse charge),
Belgium's, Italy's, Poland's and Romania's (their e-invoicing mandates: Peppol, SDI, KSeF,
e-Factura), France's (e-invoices through a plateforme agréée, e-reporting), Spain's (VERI\*FACTU
invoicing records, the recargo de equivalencia), Portugal's (certified invoices: the ATCUD, the
signed hash chain, the QR code), the United Kingdom's (VAT, the CIS reverse charge, Making Tax
Digital returns), Canada's (GST/HST, PST and QST by province), the United States' (sales and use
tax by destination, exemption certificates, AvaTax reporting, economic nexus, Forms 1099) and
thirteen more on one VAT/GST engine (South East Asia, the Gulf, Australia, New Zealand, Japan,
Mexico and China, six of them with e-invoices) are built. Point of Sale, Commerce, Assets, Field
Service, HR and CRM build on its engine.

| At a glance | |
|---|---|
| 📦 Version | `0.4.0` (the thirteen of `ferrum-countries` `0.1.0`, Singapore, Malaysia, Australia, New Zealand, Japan and the UAE `0.1.1`, the United States `0.1.0`, Canada `0.1.0`, the United Kingdom `0.1.0`, Germany `0.1.0`, Belgium, Italy, Poland, Spain, France and Portugal `0.1.0`, Romania `0.1.1`, the European Union `0.1.6`, India `0.2.1`, the other add-ons `0.1.1`) |
| 🧱 Platform | Ferrum `>=0.3.1, <0.4` |
| 🖥️ Interface | its own, at `/erp/`, and the portal's invoices at `/portal/invoices/` |
| 🗃️ Packages | accounting, selling, buying, inventory, manufacturing (`packages/`) |
| 🧩 Add-ons | the European Union, Germany, Belgium, Italy, Poland, Romania, Spain, France, Portugal, the United Kingdom, the United States, Canada, thirteen more countries, India, Finance, Procurement, Stock Operations, Manufacturing Operations (`addons/`) |
| 🌍 Countries | add-ons on the localization seam: 26 built (India, the European Union, Germany, Belgium, Italy, Poland, Romania, Spain, France, Portugal, the United Kingdom, the United States, Canada, and Singapore, Malaysia, Indonesia, Thailand, the Philippines, Vietnam, Saudi Arabia, the UAE, Australia, New Zealand, Japan, Mexico and China); the mandates of Croatia, Greece and Hungary follow ([localization.md](https://github.com/CBFerrum/Ferrum/blob/main/docs/plan/localization.md)) |
| 🔌 Built on by | Point of Sale, Commerce, Assets, Field Service, HR and CRM |
| 👥 Roles | Accounts, Sales, Purchase, Stock and Manufacturing managers and users |

> [!NOTE]
> Everything ERP is lives here: its manifest, interface, packages, engine and add-ons. The
> platform keeps only what every app shares (`ferrum-domain`: helpers, the report framework,
> document helpers and the shared packages), and the apps that post to the books or move stock
> call this crate's `ledger`, `stock`, `pricing` and `flow`.

## Modules

| | Module | What it does | Reports |
|---|---|---|---|
| 📒 | **Accounting** | the universal journal (append-only, balanced in the database), fiscal years, cost centers, accounts and tax templates, journal and payment entries with open-item allocation, period close | trial balance, general ledger, profit and loss, balance sheet, receivables, payables |
| 🧾 | **Selling** | sales orders, delivery notes and invoices with exact tax maths, pricing rules, receivables | receivables, sales register |
| 🛒 | **Buying** | purchase orders, receipts and supplier invoices with three-way matching against orders and receipts | payables, purchase register |
| 📦 | **Inventory** | warehouses and stock entries on an append-only stock ledger with moving-average and FIFO cost layers | stock balance, stock ledger |
| 🏭 | **Manufacturing** | multi-level bills of materials with revisions, work orders with actual costing, an MRP run that proposes purchases and production | stock balance |

### Add-ons

| | Add-on | What it adds |
|---|---|---|
| 🇪🇺 | **European Union** (`ferrum-eu`) | EU VAT for a company in a member state: home rates, intra-Community supplies and reverse charge with VAT numbers confirmed in VIES, the One-Stop Shop for sales to consumers elsewhere in the EU, exports and acquisitions; rates from the Commission's TEDB; EN 16931 e-invoices (Peppol BIS Billing 3.0) handed to the company's Peppol access point; the EC sales list and the OSS return |
| 🇩🇪 | **Germany** (`ferrum-de`, on the EU's) | XRechnung 3.0 e-invoices checked against KoSIT's rules (a public buyer's Leitweg-ID, a valid IBAN, both electronic addresses), small businesses (§ 19 UStG) invoicing without VAT, and the domestic reverse charge (§ 13b UStG) |
| 🇪🇺 | **Belgium, Italy, Poland, Romania, Spain, France, Portugal** (`ferrum-eu-members`, on the EU's) | Each country's e-invoicing mandate and home rules: Belgium's Peppol between businesses (enterprise numbers as 0208 IDs); Italy's FatturaPA through SDI (Natura, the virtual stamp duty, SDI's receipts); Poland's FA(3) through KSeF (corrections citing KSeF numbers); Romania's CIUS-RO uploaded to ANAF's e-Factura over the company's own connection, ANAF's verdict followed up (storno corrections); Spain's VERI\*FACTU invoicing records (each chained to the last by AEAT's SHA-256 fingerprint, with the verification QR code, sent to AEAT through the outbox), the recargo de equivalencia, and the Canary Islands, Ceuta and Melilla outside the VAT area; France's e-invoices through the company's plateforme agréée (both parties' SIRENs, the platform's statuses, corrections by avoir), e-reporting of the other sales, and the invoice's mandatory mentions; Portugal's certified invoices (each numbered in a series communicated to the AT, with its ATCUD, signed over the series' previous signature and printed with the QR code), signed with the software producer's key set on the server. Small businesses' exemptions and the domestic reverse charge in all but Spain |
| 🇬🇧 | **United Kingdom** (`ferrum-gb`) | UK VAT (20%, 5%, zero-rated, exempt), the construction industry's reverse charge (CIS), exports and services abroad, reverse-charged services from abroad, VAT numbers checked by HMRC's check digits, and VAT returns (the nine boxes) filed through HMRC's Making Tax Digital API |
| 🇺🇸 | **United States** (`ferrum-us`) | Sales tax by destination (by origin within origin-based states) at the ZIP code's combined rate from the states' rate tables, only where the company is registered; marketplace facilitators, exemption certificates and product taxability by state; use tax on untaxed purchases; sales reported to Avalara AvaTax; sales tax by jurisdiction, economic nexus and Forms 1099 reports |
| 🌏 | **Thirteen more countries** (`ferrum-countries`) | Singapore, Malaysia, Indonesia, Thailand, the Philippines, Vietnam, Saudi Arabia, the UAE, Australia, New Zealand, Japan, Mexico and China: each its own app on one VAT/GST engine, with its dated rates, exports zero-rated, input tax recovered where it is recoverable, services from abroad reverse-charged where the country says, its registration number's check and its return; e-invoices sent on submit: Peppol PINT for Singapore (InvoiceNow), Australia, New Zealand, Japan and the UAE through the company's access point, and Malaysia's through LHDN's MyInvois (below) |
| 🇨🇦 | **Canada** (`ferrum-ca`) | GST/HST, PST/RST and QST by province of supply (goods where delivered, services by the customer's address) with the CRA's dated rates, zero-rated and exempt supplies, exports, input tax credits, and the GST/HST (GST34) and QST returns |
| 🇮🇳 | **India** (`ferrum-india`) | GSTIN validation, place of supply, CGST + SGST or IGST chosen for you, HSN/SAC on lines, reverse charge on purchases (RCM), GST e-invoices (IRN, signed QR) and e-way bills through the IRP |
| 💰 | **Finance** | budgets, bank reconciliation, period closing, dunning, subscriptions and payment terms |
| 📋 | **Procurement** | material requests, requests for quotation, supplier quotations and scorecards |
| 🏷️ | **Stock Operations** | stock reconciliation, landed costs, pick lists, packing, quality inspection, item variants and product bundles |
| ⚙️ | **Manufacturing Operations** | workstations, operations, routings, job cards, production planning and subcontracting |

<details>
<summary><b>The thirteen countries of <code>ferrum-countries</code></b></summary>

| | Country | Tax | Return | E-invoices |
|---|---|---|---|---|
| 🇸🇬 | Singapore | GST 9% (8% in 2023, 7% before), the reverse charge for RC businesses | GST F5 | PINT SG through InvoiceNow (UEN) |
| 🇲🇾 | Malaysia | sales tax 10% and 5%, service tax 8% and 6%, not recovered; service tax on imported services | SST-02 | MyInvois through LHDN (UBL JSON) |
| 🇮🇩 | Indonesia | PPN 12% on 11/12 of the price, luxury goods at 12%; the 16-digit NPWP | SPT Masa PPN | — |
| 🇹🇭 | Thailand | VAT 7%; the 13-digit tax ID's check digit | PP.30 | — |
| 🇵🇭 | The Philippines | VAT 12% | 2550Q | — |
| 🇻🇳 | Vietnam | VAT 10%, 5% and the temporary 8% | 01/GTGT | — |
| 🇸🇦 | Saudi Arabia | VAT 15% (5% before July 2020) | ZATCA's VAT return | — |
| 🇦🇪 | The United Arab Emirates | VAT 5% | VAT 201, by emirate | PINT AE (the TIN from the TRN) |
| 🇦🇺 | Australia | GST 10%, GST-free and input-taxed supplies; the ABN's check | the BAS's GST labels | PINT A-NZ (ABN) |
| 🇳🇿 | New Zealand | GST 15%; the IRD number's check | GST101A | PINT A-NZ (NZBN) |
| 🇯🇵 | Japan | consumption tax 10% and the reduced 8%, once per rate per invoice (qualified invoices); the T-number | national and local parts | JP PINT (T-number) |
| 🇲🇽 | Mexico | IVA 16%, the border regions' 8%, 0% and exempt; the RFC | IVA, on a cash basis | — |
| 🇨🇳 | China | VAT 13%, 9% and 6%; small-scale taxpayers' 1% without input credit; the unified social credit code | — | — |

</details>

### Every country as an add-on

The engine knows no country. A country's add-on brings the fields it needs through its package
(prefixed with its code, such as `in_gstin`) and plugs its rules into one seam,
`ferrum_app_erp::localization`. Its rules act only where its package is installed and only for
companies of its country, so one workspace can keep companies in several countries.

| The seam | When it runs | India | European Union |
|---|---|---|---|
| `determine` | inside `pricing::calculate`, before prices and taxes: ERP's documents, CRM's quotations, POS's sales and Commerce's carts alike | the GST template for the place of supply | home, intra-EU, One-Stop Shop, export or reverse charge, from the parties' countries and VAT numbers |
| `line_rates` | after an item's own rates, line by line | — | the destination country's rate for each line (from TEDB, by CN code) |
| `check` | before a priced document is submitted | HSN/SAC codes on outward supplies | the company's VAT number, and the customer's confirmed in VIES before a supply without VAT |
| `setup_company` | after a new company's chart of accounts | GST accounts and tax templates | VAT accounts for a company in a member state |

A member state's add-on builds on the EU's: Germany's (`ferrum-de`) adds XRechnung, and
`ferrum-eu-members` brings Belgium, Italy, Poland, Romania and France, each with its e-invoicing
network, and Spain and Portugal, whose invoices form chained records, on top of the EU's VAT and
its Peppol hand-over.
E-invoices, returns, legal print content and withheld taxes use what the platform already has:
hooks and the outbox, report sources, print formats and deducted tax rows. How each region's taxes
map onto the seam is in [the plan](https://github.com/CBFerrum/Ferrum/blob/main/docs/plan/erp-repo.md#every-country-as-an-add-on).

## Screenshots

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/erp-home-dark.webp">
  <img alt="ERP's home: its modules, what is assigned to you and recent records." src="docs/assets/erp-home-light.webp" width="100%">
</picture>
<p align="center"><sub>ERP's home: its modules and their add-ons, what is assigned to you, recent records.</sub></p>

<table>
<tr>
<td width="50%">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/erp-invoice-dark.webp">
  <img alt="A submitted sales invoice with its actions: payment, credit note, e-invoice, e-way bill, print, email." src="docs/assets/erp-invoice-light.webp" width="100%">
</picture>
<p align="center"><sub>A submitted invoice: payment, credit note, GST e-invoice and e-way bill, print, email.</sub></p>
</td>
<td width="50%">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/erp-balance-sheet-dark.webp">
  <img alt="The balance sheet from the ledger, by account." src="docs/assets/erp-balance-sheet-light.webp" width="100%">
</picture>
<p align="center"><sub>The balance sheet, straight from the ledger.</sub></p>
</td>
</tr>
</table>

> [!TIP]
> The screenshots follow your GitHub theme: switch between light and dark to see both.

## How it works

### Documents and the ledgers

```mermaid
flowchart LR
    subgraph Selling
        SO["🧾 Sales order"] -->|make delivery note| DN["🚚 Delivery note"]
        DN -->|make sales invoice| SI["📄 Sales invoice"]
        SI -->|make payment| PE["💳 Payment entry"]
    end
    subgraph Buying
        PO["🛒 Purchase order"] -->|make purchase receipt| PR["📥 Purchase receipt"]
        PR -->|make purchase invoice| PI["📄 Purchase invoice"]
    end
    DN --> SL[("📦 Stock ledger<br/><small>append-only, cost layers</small>")]
    PR --> SL
    SI --> GL[("📒 Universal journal<br/><small>append-only, balanced</small>")]
    PI --> GL
    PE --> GL
    SL --> GL
    GL --> REP["📊 Trial balance · profit and loss<br/>balance sheet · receivables"]
    classDef erp fill:#2f5bea,color:#fff,stroke:#2f5bea
    class SL,GL erp
```

### Inside Ferrum

```mermaid
flowchart LR
    subgraph Browser
        UI["ERP's interface<br/><small>/erp/ · /portal/invoices/</small>"]
        BAR["Ferrum bar<br/><small>launcher · search · bell</small>"]
    end
    subgraph Server["Ferrum server"]
        API["REST · MCP · live"]
        K["Kernel<br/><small>permissions · row-level security · audit</small>"]
        ENG["ERP's engine<br/><small>ledger · stock · pricing · flow</small>"]
        ADD["Add-ons<br/><small>European Union · Germany · Belgium · Italy · Poland · Romania<br/>Spain · France · Portugal · United Kingdom · United States<br/>Canada · India · 13 more countries<br/>Finance · Procurement · Stock Operations · Manufacturing Operations</small>"]
        CORE["Shared core<br/><small>ferrum-domain</small>"]
        OB[("Outbox")]
    end
    DB[("PostgreSQL")]
    UI --> API --> K --> DB
    BAR --> API
    K <--> ENG
    K <--> ADD
    ADD -->|localization · hooks| ENG
    ENG --> CORE
    K --> OB --> EV["Notifications · webhooks<br/>automations"]
    APPS["POS · Commerce · Assets<br/>Field Service · HR · CRM"] -. post through .-> ENG
    classDef erp fill:#2f5bea,color:#fff,stroke:#2f5bea
    class UI,ENG,ADD erp
```

Everything ERP adds is ordinary Ferrum metadata and Rust: its records get lists, forms, search,
permissions, an audit trail, exports and the REST and MCP APIs for free, so AI agents work with
them under the same rules as people.

## This repository

```mermaid
flowchart BT
    KER["ferrum-kernel · meta · calc<br/><small>Ferrum</small>"]
    CORE["ferrum-domain<br/><small>Ferrum: the shared core</small>"]
    subgraph REPO["this repository"]
        APP["ferrum-app-erp<br/><small>app.yaml · packages/ · the engine · ui/dist</small>"]
        ADD["addons/<br/><small>European Union (ferrum-eu) · Germany (ferrum-de)<br/>Belgium · Italy · Poland · Romania<br/>Spain · France · Portugal (ferrum-eu-members)<br/>United Kingdom (ferrum-gb) · United States (ferrum-us)<br/>Canada (ferrum-ca) · India (ferrum-india)<br/>13 more countries (ferrum-countries)<br/>Finance · Procurement · Stock Operations<br/>Manufacturing Operations</small>"]
    end
    APPS["Point of Sale · Commerce · Assets<br/>Field Service · HR · CRM"]
    SRV["ferrum-server<br/><small>Ferrum: compiles them all in</small>"]
    FE["Ferrum-Frontend<br/><small>@ferrum/kit · client · records/frame</small>"]
    KER --> CORE --> APP
    APP --> ADD
    APP --> APPS
    ADD --> SRV
    APPS --> SRV
    FE -. builds ui/ .-> APP
    classDef erp fill:#2f5bea,color:#fff,stroke:#2f5bea
    class APP,ADD erp
```

Each arrow points from a crate to what builds on it, so nothing below ERP depends on it.

| Stage | What | Status |
|---|---|---|
| 1 | The app and its interface: manifests and `ui/`, compiled into this crate | ✅ built |
| 2 | The add-ons: Finance, Procurement, Stock Operations, Manufacturing Operations | ✅ built |
| 3 | The packages and the engine, India as an add-on on the localization seam, and the apps that build on them | ✅ built |

## Install

ERP ships in the official Ferrum server image and is recorded as installed in every workspace
that has accounting; India and the other add-ons install from **Apps** (`/store/`) in Ferrum, where
an administrator also upgrades ERP when a new version arrives. Everything it adds is available to AI
agents over Ferrum's MCP server, within each person's permissions.

## Develop

Clone it next to the platform:

```text
work/
├── ferrum/            https://github.com/CBFerrum/Ferrum (with Ferrum-Frontend cloned into ferrum/web)
└── ferrum-app-erp/
```

Point the platform dependencies at your checkout (this file is git-ignored):

```toml
# .cargo/config.toml
[patch."https://github.com/CBFerrum/Ferrum"]
ferrum-kernel = { path = "../ferrum/crates/kernel" }
ferrum-meta = { path = "../ferrum/crates/meta" }
ferrum-domain = { path = "../ferrum/crates/domain" }
ferrum-calc = { path = "../ferrum/crates/calc" }
ferrum-apps = { path = "../ferrum/crates/apps" }
ferrum-plugins = { path = "../ferrum/crates/plugins" }
```

Apps that build on ERP (Point of Sale, Commerce, Assets, Field Service, HR, CRM and the store)
take this repository from GitHub; to work on both at once, clone it beside them and add
`[patch."https://github.com/CBFerrum/ferrum-app-erp"]` with `ferrum-app-erp = { path = "../ferrum-app-erp" }`.

The tests need PostgreSQL with an ordinary (non-superuser) role, so row-level security applies,
and a database for ERP's and each add-on's (they reset it first):

```bash
createuser ferrum --pwprompt
for db in erp eu de eu_members gb ca us countries finance procurement stock_ops mfg_ops; do
  createdb -O ferrum ferrum_${db}_test
done
cargo test --workspace
```

`FERRUM_ERP_TEST_DATABASE_URL`, `FERRUM_EU_TEST_DATABASE_URL`, `FERRUM_DE_TEST_DATABASE_URL`,
`FERRUM_GB_TEST_DATABASE_URL`, `FERRUM_CA_TEST_DATABASE_URL`, `FERRUM_US_TEST_DATABASE_URL`, `FERRUM_COUNTRIES_TEST_DATABASE_URL`, `FERRUM_EU_MEMBERS_TEST_DATABASE_URL`,
`FERRUM_FINANCE_TEST_DATABASE_URL`,
`FERRUM_PROCUREMENT_TEST_DATABASE_URL`, `FERRUM_STOCK_OPS_TEST_DATABASE_URL` and
`FERRUM_MFG_OPS_TEST_DATABASE_URL` override the URLs. ERP's end-to-end test (`tests/erp_it.rs`)
walks a GST-registered company through opening stock, procure-to-pay, order-to-cash with a fake
IRP, payments, credit notes, manufacturing, MRP, period closing and concurrent stock issues, and a
company outside India that gets none of India's rules.

The interface lives in `ui/` and builds with Ferrum-Frontend's packages:

```bash
cd ui
npm run build      # ui/dist, checked against the size budget
npm test
```

With a development server started with `FERRUM_DEV_UI=1`, `ferrum-app dev --ui` serves the pages
from Vite behind the real bar and session, with hot reload.

### Project layout

```text
app.yaml         ERP's manifest: its packages, interface, launcher entry and portal page
packages/        accounting, buying, inventory, manufacturing, selling
src/             the engine (ledger, stock, pricing, trade, manufacturing, set-up, reports,
                 localization), the manifest's entry points and the embedded interface
tests/           the end-to-end scenario against a real PostgreSQL
ui/              ERP's interface (React + Vite), built into ui/dist
addons/          the European Union, Germany, the United Kingdom, the United States, Canada
                 and India (each with its app.yaml); Belgium, Italy, Poland, Romania, Spain,
                 France and Portugal (eu-members/) and thirteen more countries (countries/),
                 an app each under apps/; Finance, Procurement, Stock Operations,
                 Manufacturing Operations
CHANGELOG.md     what changed in each version
```

## Changelog

| Version | Highlights |
|---|---|
| **Portugal 0.1.0** | Portugal in `ferrum-eu-members`: invoices from certified software. Each takes the next number of a series communicated to the AT and its ATCUD, is signed (RSA) over its date, number, total and the series' previous signature, so each series forms a hash chain, and carries the QR code (Portaria 195/2020); NIFs checked; the small business exemption (article 53) and the construction reverse charge. The software producer's key and certificate number are set on the server (`FERRUM_PT_SIGNING_KEY`, `FERRUM_PT_CERTIFICATE`, `FERRUM_PT_KEY_VERSION`); without them, submitting is refused. |
| **Romania 0.1.1** | The connection to ANAF's e-Factura in `ferrum-eu-members`: OAuth sign-in with the company's qualified certificate, its tokens stored encrypted and refreshed; queued CIUS-RO invoices uploaded (test or production), and ANAF asked after each until it accepts it (the signed invoice's download ID recorded) or rejects it (with its errors). With the connection Off, invoices wait for ANAF's answer to be recorded by hand. |
| **Malaysia 0.1.1** | Malaysia's e-invoices through LHDN's MyInvois in `ferrum-countries`: a submitted invoice or credit note as UBL 2.1 JSON with the supplier's TIN, BRN, SST number and MSIC code, submitted with its SHA-256 and followed until LHDN validates it, its UUID, long ID and validation link recorded; cancelled at LHDN within 72 hours, corrected by a credit note after; the general public's and foreign buyers' TINs. With AvaTax or HMRC set to Off, the US and UK add-ons never reach the service, even with the test address override. |
| **Singapore, Australia, New Zealand, Japan, UAE 0.1.1** | Peppol PINT e-invoices in `ferrum-countries`: PINT SG for InvoiceNow, PINT A-NZ, JP PINT and PINT AE, each with its tax scheme, category codes and participants' electronic addresses (UEN, ABN, NZBN, the T-number, the TIN), checked against PINT's rules and sent through the company's Peppol access point on submit, the access point's answer recorded. The EU 0.1.6's UBL writer takes a profile, transaction type and tax scheme. |
| **France 0.1.0** | France in `ferrum-eu-members`, for the e-invoicing reform: invoices to French businesses queued on submit for the company's plateforme agréée as EN 16931 UBL with both parties' SIRENs, the platform's lifecycle statuses recorded, corrections by avoir; e-reporting of the other sales by day and rate; the invoice's mandatory mentions; SIREN, SIRET and VAT number keys checked; the franchise en base (article 293 B) and the construction reverse charge (283-2 nonies). The EU 0.1.5 writes the parties' legal registration identifiers. |
| **Spain 0.1.0** | Spain in `ferrum-eu-members`: VERI\*FACTU invoicing records chained by AEAT's SHA-256 fingerprint (AEAT's test vectors pass), cancellation records, the verification QR code, and records sent to AEAT through the outbox when the company uses VERI\*FACTU; NIFs checked; the recargo de equivalencia; the Canary Islands, Ceuta and Melilla outside the VAT area. |
| **Belgium, Italy, Poland, Romania 0.1.0** | `ferrum-eu-members` on the EU's add-on: Belgium's Peppol between businesses, Italy's FatturaPA through SDI, Poland's FA(3) through KSeF and Romania's CIUS-RO through ANAF's e-Factura, with each country's small business exemption and domestic reverse charge. The EU 0.1.4 writes the parties' country subdivisions. |
| **Thirteen countries 0.1.0** | `ferrum-countries`: Singapore, Malaysia, Indonesia, Thailand, the Philippines, Vietnam, Saudi Arabia, the UAE, Australia, New Zealand, Japan, Mexico and China, each its own app on one VAT/GST engine, with dated rates, its registration number's check and its return. |
| **United States 0.1.0** | The United States' add-on: sales tax by destination at ZIP codes' rates where the company is registered, exemption certificates, marketplace facilitators, product taxability, use tax, AvaTax reporting, and sales tax, economic nexus and Form 1099 reports. |
| **Canada 0.1.0** | Canada's add-on: GST/HST, PST/RST and QST by province of supply with the CRA's dated rates, zero-rated and exempt supplies, exports, input tax credits, and the GST/HST and QST returns. |
| **United Kingdom 0.1.0** | The UK's add-on: UK VAT with the CIS reverse charge, exports and services abroad, and VAT returns filed through HMRC's Making Tax Digital API. The EU 0.1.2 reads TEDB with an updated XML parser (two advisories fixed). |
| **Germany 0.1.0** | Germany's add-on on the EU's: XRechnung 3.0 to KoSIT's rules, small businesses (§ 19 UStG) and the domestic reverse charge (§ 13b UStG). The EU 0.1.1 puts a company's invoice address, contact, IBAN and tax number on its e-invoices. |
| **0.4.0** | The European Union's add-on (`ferrum-eu` 0.1.0): EU VAT with VIES, intra-Community supplies, the One-Stop Shop, exports and acquisitions; rates from TEDB; EN 16931 e-invoices with the Peppol hand-over; the EC sales list and the OSS return. India 0.2.1 follows NIC's reverse-charge rules on e-invoices. |
| **0.3.0** | What every country's rules build on: tax categories and exemption reasons with a tax breakdown per category and rate (EN 16931), taxes per unit, reverse charge, tax rounding line by line, and party and ship-to addresses on documents. India 0.2.0 follows a company's ISO country code and handles reverse-charge purchases. |
| **0.2.0** | ERP's own repository holds all of it: its manifest, interface, packages and engine, which other apps build on, and its add-ons. Countries plug in through `localization`, India first as the add-on `ferrum-india`. Earlier history is in Ferrum and Ferrum-Frontend. |

Every version is in [CHANGELOG.md](CHANGELOG.md).

## License

[AGPL-3.0-or-later](LICENSE), like the platform. Part of [Ferrum](https://github.com/CBFerrum/Ferrum) ·
[Report an issue](https://github.com/CBFerrum/ferrum-app-erp/issues)
