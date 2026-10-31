<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
  <img alt="Ferrum ERP" src="docs/assets/banner-light.svg" width="100%">
</picture>

[![CI](https://github.com/CBFerrum/ferrum-app-erp/actions/workflows/ci.yml/badge.svg)](https://github.com/CBFerrum/ferrum-app-erp/actions/workflows/ci.yml) ![Version](https://img.shields.io/badge/version-0.2.0-2f5bea?style=flat-square) ![Ferrum](https://img.shields.io/badge/Ferrum-%3E%3D0.3.1%2C%20%3C0.4-2f5bea?style=flat-square) ![License](https://img.shields.io/badge/license-AGPL--3.0--or--later-3b82f6?style=flat-square) ![Rust](https://img.shields.io/badge/Rust-native%20app-b7410e?style=flat-square&logo=rust&logoColor=white) ![Interface](https://img.shields.io/badge/interface-React%20%2B%20Vite-0ea5e9?style=flat-square&logo=react&logoColor=white) ![MCP](https://img.shields.io/badge/AI%20agents-MCP-8b5cf6?style=flat-square)

**A native app for [Ferrum](https://github.com/CBFerrum/Ferrum), the open-source, AI-first business platform.**

[Overview](#overview) · [Modules](#modules) · [Screenshots](#screenshots) · [How it works](#how-it-works) · [Repository](#this-repository) · [Install](#install) · [Develop](#develop) · [Changelog](#changelog)

</div>

---

## Overview

Ferrum ERP is accounting, selling, buying, stock and manufacturing on one universal journal that
is append-only and balanced in the database. Documents carry exact tax maths, stock moves through
an append-only ledger with moving-average and FIFO cost layers (a backdated entry posts forward;
nothing is ever reposted), and every figure the reports show comes from the same ledger. Its
add-ons bring budgets and bank reconciliation, procurement, stock operations and manufacturing
operations, and India's localisation brings GST, e-invoices and e-way bills.

| At a glance | |
|---|---|
| 📦 Version | `0.2.0` (its add-ons `0.1.1`) |
| 🧱 Platform | Ferrum `>=0.3.1, <0.4` |
| 🖥️ Interface | its own, at `/erp/`, and the portal's invoices at `/portal/invoices/` |
| 🗃️ Packages | accounting, selling, buying, inventory, manufacturing; India as its add-on |
| 🧩 Add-ons | Finance, Procurement, Stock Operations, Manufacturing Operations |
| 👥 Roles | Accounts, Sales, Purchase, Stock and Manufacturing managers and users |

> [!NOTE]
> ERP's packages and their logic still live in the platform (`ferrum-domain`), which other apps
> build on: Point of Sale, Commerce, payroll, Assets, Bookings and Field Service post through
> it. They move here in stage 3 of [the plan](https://github.com/CBFerrum/Ferrum/blob/main/docs/plan/erp-repo.md).

## Modules

| | Module | What it does | Reports |
|---|---|---|---|
| 📒 | **Accounting** | the universal journal (append-only, balanced in the database), fiscal years, cost centers, accounts and tax templates, journal and payment entries with open-item allocation, period close | trial balance, general ledger, profit and loss, balance sheet, receivables, payables |
| 🧾 | **Selling** | sales orders, delivery notes and invoices with exact tax maths, pricing rules, receivables | receivables, sales register |
| 🛒 | **Buying** | purchase orders, receipts and supplier invoices with three-way matching against orders and receipts | payables, purchase register |
| 📦 | **Inventory** | warehouses and stock entries on an append-only stock ledger with moving-average and FIFO cost layers | stock balance, stock ledger |
| 🏭 | **Manufacturing** | multi-level bills of materials with revisions, work orders with actual costing, an MRP run that proposes purchases and production | stock balance |
| 🇮🇳 | **India** | GSTIN validation, place of supply, CGST + SGST or IGST chosen for you, HSN on lines, GST e-invoices (IRN, signed QR) and e-way bills | |

### Add-ons

| | Add-on | What it adds |
|---|---|---|
| 💰 | **Finance** | budgets, bank reconciliation, period closing, dunning, subscriptions and payment terms |
| 📋 | **Procurement** | material requests, requests for quotation, supplier quotations and scorecards |
| 🏷️ | **Stock Operations** | stock reconciliation, landed costs, pick lists, packing, quality inspection, item variants and product bundles |
| ⚙️ | **Manufacturing Operations** | workstations, operations, routings, job cards, production planning and subcontracting |

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
        DOM["Business engine<br/><small>ferrum-domain: ERP's packages and logic</small>"]
        ADD["ERP's add-ons<br/><small>this repository</small>"]
        OB[("Outbox")]
    end
    DB[("PostgreSQL")]
    UI --> API --> K --> DB
    BAR --> API
    K <--> DOM
    K <--> ADD
    K --> OB --> EV["Notifications · webhooks<br/>automations"]
    APPS["POS · Commerce · HR payroll<br/>Assets · Bookings · Field Service"] -. post through .-> DOM
    classDef erp fill:#2f5bea,color:#fff,stroke:#2f5bea
    class UI,ADD erp
```

Everything ERP adds is ordinary Ferrum metadata and Rust: its records get lists, forms, search,
permissions, an audit trail, exports and the REST and MCP APIs for free, so AI agents work with
them under the same rules as people.

## This repository

```mermaid
flowchart TB
    subgraph REPO["ferrum-app-erp"]
        APP["ferrum-app-erp crate<br/><small>app.yaml · india/app.yaml · ui/dist embedded</small>"]
        F["addons/finance"]
        P["addons/procurement"]
        S["addons/stock-ops"]
        M["addons/mfg-ops"]
    end
    subgraph FERRUM["Ferrum (platform)"]
        KER["ferrum-kernel · meta · calc"]
        DOM2["ferrum-domain<br/><small>ERP's packages and logic until stage 3</small>"]
        SRV["ferrum-server<br/><small>compiles them in</small>"]
    end
    FE["Ferrum-Frontend<br/><small>@ferrum/kit · client · records/frame</small>"]
    APP --> SRV
    F & P & S & M --> SRV
    F & P & S & M --> DOM2
    DOM2 --> KER
    FE -. builds ui/ .-> APP
```

| Stage | What | Status |
|---|---|---|
| 1 | The app and its interface: manifests and `ui/`, compiled into this crate | ✅ built |
| 2 | The add-ons: Finance, Procurement, Stock Operations, Manufacturing Operations | ✅ built |
| 3 | The packages and the engine, with the apps that build on them | 🗺️ planned ([erp-repo.md](https://github.com/CBFerrum/Ferrum/blob/main/docs/plan/erp-repo.md)) |

## Install

ERP ships in the official Ferrum server image and is recorded as installed in every workspace
that has accounting; its add-ons and India install from **Apps** (`/store/`) in Ferrum, where an
administrator also upgrades ERP when a new version arrives. Everything it adds is available to AI
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
```

The add-ons' tests need PostgreSQL with an ordinary (non-superuser) role, so row-level security
applies, and a database each (they reset it first):

```bash
createuser ferrum --pwprompt
for db in finance procurement stock_ops mfg_ops; do createdb -O ferrum ferrum_${db}_test; done
cargo test --workspace
```

`FERRUM_FINANCE_TEST_DATABASE_URL`, `FERRUM_PROCUREMENT_TEST_DATABASE_URL`,
`FERRUM_STOCK_OPS_TEST_DATABASE_URL` and `FERRUM_MFG_OPS_TEST_DATABASE_URL` override the URLs.

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
india/app.yaml   India's manifest, ERP's add-on
src/             the crate: the manifests and the embedded interface
ui/              ERP's interface (React + Vite), built into ui/dist
addons/          Finance, Procurement, Stock Operations, Manufacturing Operations
CHANGELOG.md     what changed in each version
```

## Changelog

| Version | Highlights |
|---|---|
| **0.2.0** | ERP has a repository of its own: its manifests, its interface (now on `@ferrum/records/frame`) and its add-ons, with their tests. Earlier history is in Ferrum and Ferrum-Frontend. |

Every version is in [CHANGELOG.md](CHANGELOG.md).

## License

[AGPL-3.0-or-later](LICENSE), like the platform. Part of [Ferrum](https://github.com/CBFerrum/Ferrum) ·
[Report an issue](https://github.com/CBFerrum/ferrum-app-erp/issues)
