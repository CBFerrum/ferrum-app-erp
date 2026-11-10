//! The print layouts of ERP's everyday documents, rendered for real posted documents against a real
//! PostgreSQL (`FERRUM_ERP_PRINT_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_print_test`; reset first).
//!
//! An invoice with two taxes, a discount, a line without an item and a foreign currency, the credit
//! and debit notes against it, a pro forma, deliveries, purchases, payments of every kind, a journal
//! voucher and stock entries: each is printed through the platform's `render_print` and checked for
//! what a customer or an auditor reads on it, and for what must not be there (an empty block, an
//! unescaped name). Set `PRINT_SAMPLES_DIR` to keep the rendered pages for a look.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

/// How a page prints an amount: grouped thousands, `p` decimals, no sign.
fn money(v: &Value, p: u32) -> String {
    let d = Decimal::from_str(v.as_str().unwrap_or_else(|| panic!("not a decimal string: {v}"))).unwrap().abs();
    let s = d.round_dp_with_strategy(p, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    let s = format!("{:.*}", p as usize, s);
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if frac.is_empty() { out } else { format!("{out}.{frac}") }
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
    samples: Option<std::path::PathBuf>,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn make(&self, entity: &str, v: Value) -> Doc {
        let d = self.insert(entity, v).await;
        self.submit(entity, uid(&d)).await
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Uuid {
        let v = self.k.run_action(&self.ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"));
        vid(&v)
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    /// The printed page of a document, kept under `PRINT_SAMPLES_DIR/<name>.html` when asked.
    async fn print(&self, entity: &str, id: Uuid, name: &str) -> String {
        let html = self.k.render_print(&self.ctx, entity, id).await.unwrap_or_else(|e| panic!("print {entity}: {e:?}"));
        if let Some(dir) = &self.samples {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{name}.html")), &html).unwrap();
        }
        html
    }
}

/// Every part is on the page (the engine escapes a slash as `&#x2f;`: it reads as the slash).
#[track_caller]
fn has(html: &str, what: &str, parts: &[&str]) {
    let page = html.replace("&#x2f;", "/");
    for p in parts {
        assert!(page.contains(&p.replace("&#x2f;", "/")), "{what}: {p:?} is not on the page:\n{}", body_text(html));
    }
}

/// None of the parts is on the page.
#[track_caller]
fn lacks(html: &str, what: &str, parts: &[&str]) {
    let page = html.replace("&#x2f;", "/");
    for p in parts {
        assert!(!page.contains(&p.replace("&#x2f;", "/")), "{what}: {p:?} must not be on the page:\n{}", body_text(html));
    }
}

/// The page without its stylesheet, for a failure's message.
fn body_text(html: &str) -> &str {
    html.split("</style>").nth(1).unwrap_or(html)
}

#[test]
fn print_layouts() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_PRINT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_print_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "pl".into(),
            name: "Print Co".into(),
            admin_email: "admin@pl.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("pl", "admin@pl.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx, samples: std::env::var_os("PRINT_SAMPLES_DIR").map(Into::into) };

    // ───────────── a company with its letterhead, parties, items and taxes ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let usd = t.find_one("Currency", json!({"code": "USD"})).await;
    let cid = uid(
        &t.insert(
            "Company",
            json!({"company_name": "Print Co", "abbr": "PC", "default_currency": inr["id"], "country": "India", "tax_id": "TIN-4410",
                   "acc_address_line1": "12 Harbour Road", "acc_address_line2": "Unit 4", "acc_city": "Mumbai", "acc_region": "Maharashtra", "acc_postal_code": "400001",
                   "acc_phone": "+91 22 5550 1234", "acc_email": "accounts@printco.test", "acc_website": "printco.test", "acc_logo": "https://assets.printco.test/logo.png",
                   "acc_bank_account_name": "Print Co Ltd", "acc_bank_name": "First Harbour Bank", "acc_bank_account_no": "IBAN IN00 FHB0 1234 5678", "acc_bank_branch_code": "FHBKINBB",
                   "acc_print_terms": "Goods remain ours until paid.", "acc_print_footer": "Print Co Ltd, registered in Mumbai No. 99120"}),
        )
        .await,
    );
    let c = cid.to_string();
    let stores = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await).to_string();
    let dispatch = uid(&t.insert("Warehouse", json!({"warehouse_name": "Dispatch", "company": c})).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, more: Value| {
        let mut v = json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"]});
        v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        v
    };
    let widget = t.insert("Item", item("Widget", json!({"standard_rate": "150"}))).await["id"].as_str().unwrap().to_string();
    let advice = t.insert("Item", item("Advice", json!({"standard_rate": "100", "is_stock_item": false}))).await["id"].as_str().unwrap().to_string();
    let account = |name: &'static str| {
        let t = &t;
        let c = c.clone();
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].as_str().unwrap().to_string() }
    };
    let (output, input, bank, cash, debtors, obe) = (
        account("Output Tax").await,
        account("Input Tax").await,
        account("Primary Bank").await,
        account("Cash").await,
        account("Debtors").await,
        account("Opening Balance Equity").await,
    );
    let two_taxes = t
        .insert(
            "TaxTemplate",
            json!({"title": "VAT and surcharge", "company": c, "kind": "Sales", "rows": [
                {"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "10"},
                {"charge_type": "On Net Total", "account": input, "description": "Surcharge", "rate": "2"}]}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let globex = t.insert("Customer", json!({"customer_name": "Globex", "tax_id": "GLB-100", "email": "ap@globex.test", "payment_terms_days": 30})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.insert("Address", json!({"address_type": "Billing", "address_line1": "1 Billing Lane", "city": "Pune", "postal_code": "411001", "country": "India", "is_primary": true, "customer": globex})).await;
    t.insert(
        "Address",
        json!({"address_type": "Shipping", "address_line1": "9 Dock Street", "city": "Nashik", "country": "India", "is_primary": true, "customer": globex}),
    )
    .await;
    let overseas = t.insert("Customer", json!({"customer_name": "Overseas Ltd", "default_currency": usd["id"]})).await["id"].as_str().unwrap().to_string();
    t.insert("ExchangeRate", json!({"from_currency": usd["id"], "to_currency": inr["id"], "rate": "84", "valid_from": "2000-01-01"})).await;
    let acme =
        t.insert("Supplier", json!({"supplier_name": "Acme Steel", "tax_id": "ACM-7", "address": "5 Foundry Way\nPune", "payment_terms_days": 45})).await["id"]
            .as_str()
            .unwrap()
            .to_string();
    t.make(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "100", "basic_rate": "100", "target_warehouse": stores}]}),
    )
    .await;

    // ───────────── an invoice: two taxes, a discount, a line with no item, dollars ─────────────
    let si = t
        .make(
            "SalesInvoice",
            json!({"company": c, "customer": overseas, "conversion_rate": "83", "tax_template": two_taxes, "discount_percent": "5", "po_no": "PO-7781",
                   "items": [{"item": widget, "qty": "3", "discount_percent": "10", "warehouse": stores},
                             {"item": advice, "qty": "2"},
                             {"description": "Setup fee for the Pune site", "qty": "1", "price_list_rate": "40", "income_account": account("Other Income").await}]}),
        )
        .await;
    let si_id = uid(&si);
    let number = si["number"].as_str().unwrap().to_string();
    let invoice = t.print("SalesInvoice", si_id, "invoice").await;
    let grand = if si["rounded_total"].as_str().is_some_and(|r| r.parse::<f64>().unwrap_or(0.0) != 0.0) { &si["rounded_total"] } else { &si["grand_total"] };
    has(
        &invoice,
        "the invoice",
        &[
            "<h1>Invoice</h1>",
            &number,
            "Overseas Ltd",
            "PO-7781",
            "Setup fee for the Pune site",
            "Widget",
            "Disc. %",
            "VAT",
            "Surcharge",
            "10%",
            "2%",
            &format!("<td>Total USD</td><td>{}</td>", money(grand, 2)),
            &format!("<td>-{}</td>", money(&si["discount_amount"], 2)),
            &money(&si["total_taxes"], 2),
            &money(&si["net_total"], 2),
            "Exchange rate: 1 USD = 83 INR",
            &money(&si["base_grand_total"], 2),
            "Balance due",
            // The letterhead, set once on the company.
            "<img class=\"logo\" src=\"https:&#x2f;&#x2f;assets.printco.test&#x2f;logo.png\"",
            "12 Harbour Road\nUnit 4",
            "Mumbai, Maharashtra 400001",
            "+91 22 5550 1234 · accounts@printco.test · printco.test",
            "TIN-4410",
            "First Harbour Bank",
            "IBAN IN00 FHB0 1234 5678",
            "FHBKINBB",
            "Goods remain ours until paid.",
            "registered in Mumbai No. 99120",
        ],
    );
    lacks(&invoice, "the invoice", &["Credit Note", "Debit Note", "Draft", "Cancelled", "Price only", "Prices include tax"]);
    assert_eq!(invoice.matches("<tr class=\"grand\">").count(), 1);
    // The totals block and the table header are kept whole across pages.
    has(&invoice, "the invoice's print rules", &["thead{display:table-header-group}", "break-inside:avoid", "@page{margin:12mm}", "text-align:end"]);
    lacks(&invoice, "the invoice is offline", &["<script", "http://fonts", "https://fonts", "@import", "<link "]);

    // ───────────── a domestic invoice, part paid; billing and shipping addresses, terms, a receipt ─────────────
    let home = t
        .make(
            "SalesInvoice",
            json!({"company": c, "customer": globex, "tax_template": two_taxes, "items": [{"item": widget, "qty": "4", "warehouse": stores}, {"item": advice, "qty": "1"}]}),
        )
        .await;
    let home_id = uid(&home);
    let pe = t.action("SalesInvoice", home_id, "make_payment", json!({})).await;
    let line = t.get("PaymentEntry", pe).await["references"][0]["id"].clone();
    t.update("PaymentEntry", pe, json!({"paid_amount": "200", "reference_no": "CHQ-4411", "references": [{"id": line, "allocated_amount": "200"}]})).await;
    t.submit("PaymentEntry", pe).await;
    let home_page = t.print("SalesInvoice", home_id, "invoice-part-paid").await;
    let owed = t.get("SalesInvoice", home_id).await["outstanding_amount"].clone();
    has(
        &home_page,
        "the domestic invoice",
        &[
            "Bill to",
            "1 Billing Lane",
            "Pune 411001",
            "Ship to",
            "9 Dock Street",
            "GLB-100",
            "ap@globex.test",
            "<dt>Payment terms</dt><dd>30 days</dd>",
            "<td>Paid or credited</td><td>200.00</td>",
            &format!("<td>Balance due</td><td>{}</td>", money(&owed, 2)),
            "Total INR",
        ],
    );
    lacks(&home_page, "the domestic invoice", &["Exchange rate", "Customer PO"]);
    let receipt = t.print("PaymentEntry", pe, "receipt").await;
    has(
        &receipt,
        "the receipt",
        &[
            "<h1>Receipt</h1>",
            "Received from",
            "Globex",
            "GLB-100",
            "Paid into",
            "Primary Bank",
            "CHQ-4411",
            "Amount INR",
            "200.00",
            "Settles these invoices",
            home["number"].as_str().unwrap(),
        ],
    );
    lacks(&receipt, "the receipt", &["On account", "Deductions", "Exchange rate"]);

    // ───────────── a credit note against it, a price-only credit note, a debit note ─────────────
    let cn = t.action("SalesInvoice", si_id, "make_credit_note", json!({})).await;
    let cn_doc = t.submit("SalesInvoice", cn).await;
    let credit = t.print("SalesInvoice", cn, "credit-note").await;
    has(
        &credit,
        "the credit note",
        &[
            "<h1>Credit Note</h1>",
            cn_doc["number"].as_str().unwrap(),
            "Credit note against",
            &format!("Invoice {number}"),
            "Setup fee for the Pune site",
            &format!("<td>Total USD</td><td>{}</td>", money(&cn_doc["grand_total"], 2)),
        ],
    );
    lacks(&credit, "the credit note", &["Bank details", "Due ", "Balance due", "Price only"]);
    assert!(!credit.contains(&format!("-{}", money(&cn_doc["grand_total"], 2))), "a credit note prints what is credited, without a sign");
    let price_cn = t.action("SalesInvoice", home_id, "make_credit_note", json!({"credit_percent": "10"})).await;
    let price_page = t.print("SalesInvoice", t.submit("SalesInvoice", price_cn).await["id"].as_str().unwrap().parse().unwrap(), "credit-note-price-only").await;
    has(&price_page, "the price-only credit note", &["<h1>Credit Note</h1>", "Price only: no goods move"]);
    let dn_note = t.action("SalesInvoice", si_id, "make_debit_note", json!({"charge_percent": "5"})).await;
    let debit_doc = t.submit("SalesInvoice", dn_note).await;
    let debit = t.print("SalesInvoice", dn_note, "debit-note").await;
    has(
        &debit,
        "the debit note",
        &["<h1>Debit Note</h1>", "Debit note against", &format!("Invoice {number}"), "Bank details", &money(&debit_doc["grand_total"], 2)],
    );
    lacks(&debit, "the debit note", &["Credit Note", "<h1>Invoice</h1>"]);
    // A credit note with no original gives its reason.
    let goodwill = t
        .make("SalesInvoice", json!({"company": c, "customer": globex, "is_return": true, "return_reason": "Goodwill after a late delivery", "items": [{"item": advice, "qty": "-1"}]}))
        .await;
    let goodwill_page = t.print("SalesInvoice", uid(&goodwill), "credit-note-no-original").await;
    has(
        &goodwill_page,
        "the credit note without an original",
        &["<h1>Credit Note</h1>", "<dt>Reason</dt><dd class=\"pre\" dir=\"auto\">Goodwill after a late delivery</dd>"],
    );
    lacks(&goodwill_page, "the credit note without an original", &["Credit note against"]);
    // Paid, credited, refunded: the refund and its voucher.
    let paid = t.make("SalesInvoice", json!({"company": c, "customer": globex, "items": [{"item": advice, "qty": "3"}]})).await;
    let paid_pe = t.action("SalesInvoice", uid(&paid), "make_payment", json!({})).await;
    t.submit("PaymentEntry", paid_pe).await;
    let paid_cn = t.action("SalesInvoice", uid(&paid), "make_credit_note", json!({})).await;
    t.submit("SalesInvoice", paid_cn).await;
    let refund_id = t.action("SalesInvoice", paid_cn, "make_refund", json!({})).await;
    let refund_doc = t.submit("PaymentEntry", refund_id).await;
    let refund = t.print("PaymentEntry", refund_id, "refund").await;
    has(&refund, "the refund", &["<h1>Refund</h1>", "Paid to", "Globex", "Refund of credit", &money(&refund_doc["paid_amount"], 2), "Paid from"]);
    let credit_open = t.print("SalesInvoice", paid_cn, "credit-note-refunded").await;
    lacks(&credit_open, "the refunded credit note", &["Credit not yet settled"]);

    // ───────────── a pro forma, an order, deliveries ─────────────
    let order_lines = json!([{"item": widget, "qty": "5"}, {"item": advice, "qty": "2", "discount_percent": "10"}]);
    let proforma = t
        .make("SalesOrder", json!({"company": c, "customer": globex, "delivery_date": "2027-01-15", "po_no": "PO-55", "tax_template": two_taxes, "print_as_proforma": true, "set_warehouse": stores, "items": order_lines}))
        .await;
    let pf_page = t.print("SalesOrder", uid(&proforma), "pro-forma").await;
    has(
        &pf_page,
        "the pro forma",
        &["<h1>Pro Forma Invoice</h1>", "not a tax invoice", "Bank details", "First Harbour Bank", "PO-55", "2027-01-15", &money(&proforma["grand_total"], 2)],
    );
    lacks(&pf_page, "the pro forma", &["<h1>Sales Order</h1>", "Balance due"]);
    let order = t.make("SalesOrder", json!({"company": c, "customer": globex, "set_warehouse": stores, "items": order_lines})).await;
    let order_page = t.print("SalesOrder", uid(&order), "sales-order").await;
    has(&order_page, "the sales order", &["<h1>Sales Order</h1>", "Globex", "Widget", "Terms", "Goods remain ours until paid."]);
    lacks(&order_page, "the sales order", &["Pro Forma", "Bank details", "tax invoice"]);
    let dn = t.action("SalesOrder", uid(&order), "make_delivery_note", json!({})).await;
    t.submit("DeliveryNote", dn).await;
    let dn_page = t.print("DeliveryNote", dn, "delivery-note").await;
    has(&dn_page, "the delivery note", &["<h1>Delivery Note</h1>", "Customer", "Globex", "Deliver to", "9 Dock Street", "Widget", "Received by"]);
    lacks(&dn_page, "the delivery note", &["Rate", "Amount", "Total INR", "<td>Tax</td>", "Balance due"]);
    let dn_priced = t
        .make("DeliveryNote", json!({"company": c, "customer": globex, "print_prices": true, "transporter": "Swift Freight", "vehicle_no": "MH12 AB 3456", "tax_template": two_taxes, "items": [{"item": widget, "qty": "2", "warehouse": stores}]}))
        .await;
    let dn_priced_page = t.print("DeliveryNote", uid(&dn_priced), "delivery-note-with-prices").await;
    has(
        &dn_priced_page,
        "the delivery note with prices",
        &["<th class=\"num\">Rate</th>", "<th class=\"num\">Amount</th>", "Total INR", "Swift Freight", "MH12 AB 3456", &money(&dn_priced["grand_total"], 2)],
    );
    let dn_return = t.action("DeliveryNote", dn, "make_sales_return", json!({})).await;
    t.submit("DeliveryNote", dn_return).await;
    let dn_return_page = t.print("DeliveryNote", dn_return, "delivery-return").await;
    has(&dn_return_page, "the return", &["<h1>Return</h1>", "Return of", "Delivery note DN-"]);
    lacks(&dn_return_page, "the return", &["-5", "<h1>Delivery Note</h1>"]);

    // ───────────── buying: an order, its receipt, the supplier's bill, a debit note, a payment ─────────────
    let po = t
        .make("PurchaseOrder", json!({"company": c, "supplier": acme, "schedule_date": "2027-02-01", "terms": "Delivery to the stores door only.", "set_warehouse": stores, "ship_to": null, "items": [{"item": widget, "qty": "10", "price_list_rate": "95", "schedule_date": "2027-02-01"}]}))
        .await;
    let po_page = t.print("PurchaseOrder", uid(&po), "purchase-order").await;
    has(
        &po_page,
        "the purchase order",
        &[
            "<h1>Purchase Order</h1>",
            "Acme Steel",
            "ACM-7",
            "5 Foundry Way\nPune",
            "Required by",
            "2027-02-01",
            "<dt>Payment terms</dt><dd>45 days</dd>",
            "Delivery to the stores door only.",
            &money(&po["grand_total"], 2),
        ],
    );
    lacks(&po_page, "the purchase order", &["Bank details", "Balance due"]);
    let pr = t.action("PurchaseOrder", uid(&po), "make_purchase_receipt", json!({})).await;
    t.update("PurchaseReceipt", pr, json!({"supplier_delivery_note": "DN-ACME-31"})).await;
    let pr_doc = t.submit("PurchaseReceipt", pr).await;
    let pr_page = t.print("PurchaseReceipt", pr, "purchase-receipt").await;
    has(&pr_page, "the purchase receipt", &["<h1>Purchase Receipt</h1>", "DN-ACME-31", "Widget", &money(&pr_doc["grand_total"], 2), "Received by"]);
    let pi = t.action("PurchaseReceipt", pr, "make_purchase_invoice", json!({})).await;
    t.update("PurchaseInvoice", pi, json!({"bill_no": "ACME/2026/118", "bill_date": "2026-11-02"})).await;
    let pi_doc = t.submit("PurchaseInvoice", pi).await;
    let pi_page = t.print("PurchaseInvoice", pi, "purchase-invoice").await;
    has(
        &pi_page,
        "the purchase invoice",
        &[
            "<h1>Purchase Invoice</h1>",
            "ACME/2026/118",
            "2026-11-02",
            "Supplier invoice no",
            "the supplier's bill",
            "Balance due",
            &money(&pi_doc["grand_total"], 2),
        ],
    );
    lacks(&pi_page, "the purchase invoice", &["Bank details", "Debit Note"]);
    let voucher = t.action("PurchaseInvoice", pi, "make_payment", json!({})).await;
    t.submit("PaymentEntry", voucher).await;
    let voucher_page = t.print("PaymentEntry", voucher, "payment-voucher").await;
    has(
        &voucher_page,
        "the payment voucher",
        &["<h1>Payment Voucher</h1>", "Paid to", "Acme Steel", "Paid from", "Authorised by", pi_doc["number"].as_str().unwrap(), "Settles these invoices"],
    );
    let pdn = t.action("PurchaseInvoice", pi, "make_debit_note", json!({})).await;
    t.update("PurchaseInvoice", pdn, json!({"bill_no": "ACME/2026/118-DN"})).await;
    t.submit("PurchaseInvoice", pdn).await;
    let pdn_page = t.print("PurchaseInvoice", pdn, "purchase-debit-note").await;
    has(&pdn_page, "the supplier debit note", &["<h1>Debit Note</h1>", "Debit note against", "Invoice "]);

    // ───────────── a transfer, a journal voucher, stock entries ─────────────
    let transfer = t.make("PaymentEntry", json!({"company": c, "payment_type": "Internal Transfer", "bank_account": bank, "paid_to": cash, "paid_amount": "1000", "reference_no": "CHQ-1", "currency": inr["id"]})).await;
    let transfer_page = t.print("PaymentEntry", uid(&transfer), "transfer").await;
    has(&transfer_page, "the transfer", &["<h1>Transfer</h1>", "From account", "Primary Bank", "To account", "Cash", "1,000.00", "CHQ-1"]);
    lacks(&transfer_page, "the transfer", &["Received from", "Paid to", "Settles"]);
    let je = t
        .make(
            "JournalEntry",
            json!({"company": c, "entry_type": "Opening Entry", "remark": "Balances brought from the old books", "accounts": [
                {"account": debtors, "party_type": "Customer", "customer": overseas, "currency": usd["id"], "exchange_rate": "84", "debit_in_currency": "50", "reference_no": "OLD-USD-3", "due_date": "2026-12-01"},
                {"account": obe, "credit": "4200"}]}),
        )
        .await;
    let je_page = t.print("JournalEntry", uid(&je), "journal-voucher").await;
    has(
        &je_page,
        "the journal voucher",
        &[
            "Journal Voucher",
            "Opening Entry",
            "Debtors",
            "Opening Balance Equity",
            "Overseas Ltd",
            "OLD-USD-3",
            "Due 2026-12-01",
            "Dr 50.00",
            "4,200.00",
            "Balances brought from the old books",
            "Approved by",
        ],
    );
    assert_eq!(je_page.matches("4,200.00").count(), 4, "each line and the totals, on both sides");
    let receipt_se = t.make("StockEntry", json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "20", "basic_rate": "110", "target_warehouse": stores, "batch_no": "B-7"}]})).await;
    let receipt_page = t.print("StockEntry", uid(&receipt_se), "stock-receipt").await;
    has(&receipt_page, "the material receipt", &["<h1>Material Receipt</h1>", "To warehouse", "Stores", "B-7", "2,200.00", "Stores keeper"]);
    lacks(&receipt_page, "the material receipt", &["From warehouse"]);
    let moved = t.make("StockEntry", json!({"company": c, "purpose": "Material Transfer", "items": [{"item": widget, "qty": "5", "source_warehouse": stores, "target_warehouse": dispatch}]})).await;
    let moved_page = t.print("StockEntry", uid(&moved), "stock-transfer").await;
    has(&moved_page, "the material transfer", &["<h1>Material Transfer</h1>", "From warehouse", "To warehouse", "Dispatch", "Stores"]);

    // ───────────── drafts, cancelled documents, markup in a name ─────────────
    let draft = t.insert("SalesInvoice", json!({"company": c, "customer": globex, "items": [{"item": advice, "qty": "1"}]})).await;
    let draft_page = t.print("SalesInvoice", uid(&draft), "invoice-draft").await;
    has(&draft_page, "a draft", &["<span class=\"mark\">Draft</span>", "<h1>Invoice</h1>"]);
    lacks(&draft_page, "a draft", &["Balance due", "<div class=\"strong\"></div>"]);
    let dead = t.make("SalesInvoice", json!({"company": c, "customer": globex, "items": [{"item": advice, "qty": "1"}]})).await;
    t.k.cancel(&t.ctx, "SalesInvoice", uid(&dead), None).await.expect("cancel");
    let dead_page = t.print("SalesInvoice", uid(&dead), "invoice-cancelled").await;
    has(&dead_page, "a cancelled invoice", &["<span class=\"mark\">Cancelled</span>"]);
    lacks(&dead_page, "a cancelled invoice", &["<span class=\"mark\">Draft</span>", "Balance due"]);
    let mallory = t
        .insert(
            "Customer",
            json!({"customer_name": "Mal <script>alert(1)</script> & Sons", "tax_id": "<b>X</b>", "billing_address": "<img src=x onerror=alert(2)>"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let evil = t.make("SalesInvoice", json!({"company": c, "customer": mallory, "po_no": "\"><svg onload=1>", "items": [{"description": "<i>fancy</i> job", "qty": "1", "price_list_rate": "10", "income_account": account("Other Income").await}]})).await;
    let evil_page = t.print("SalesInvoice", uid(&evil), "invoice-markup").await;
    has(
        &evil_page,
        "markup in names",
        &[
            "Mal &lt;script&gt;alert(1)&lt;&#x2f;script&gt; &amp; Sons",
            "&lt;b&gt;X&lt;&#x2f;b&gt;",
            "&lt;img src=x onerror=alert(2)&gt;",
            "&lt;i&gt;fancy&lt;&#x2f;i&gt; job",
            "&lt;svg onload=1&gt;",
        ],
    );
    lacks(&evil_page, "markup in names", &["<script>", "<b>X", "<img src=x", "<i>fancy", "<svg onload"]);
    // The company's own letterhead is typed text too.
    t.update("Company", cid, json!({"acc_print_footer": "<b>Thanks</b> & regards"})).await;
    let footer_page = t.print("SalesInvoice", si_id, "invoice").await;
    has(&footer_page, "an escaped footer", &["&lt;b&gt;Thanks&lt;&#x2f;b&gt; &amp; regards"]);

    // ───────────── a company that has filled in nothing: no empty blocks ─────────────
    let bare = uid(&t.insert("Company", json!({"company_name": "Bare Co", "abbr": "BC", "default_currency": inr["id"]})).await).to_string();
    let plain = t.make("SalesInvoice", json!({"company": bare, "customer": mallory.clone(), "items": [{"description": "Consulting", "qty": "2", "price_list_rate": "500", "income_account": account_in(&t, "Other Income", &bare).await}]})).await;
    let plain_page = t.print("SalesInvoice", uid(&plain), "invoice-bare-company").await;
    has(&plain_page, "the bare invoice", &["Bare Co", "Consulting", "1,000.00", "Total INR"]);
    lacks(
        &plain_page,
        "the bare invoice",
        &[
            "<img",
            "Bank details",
            "<h2>Terms</h2>",
            "Tax ID</span> </div>",
            "Ship to",
            "dir=\"auto\"></div>",
            "<div class=\"pre\" dir=\"auto\"></div>",
            "<dd dir=\"auto\"></dd>",
            "class=\"muted\" dir=\"auto\"></div>",
            "Remarks",
            "Prices include tax",
            "Exchange rate",
        ],
    );
    // and none of the other layouts print an empty heading either.
    for (entity, id) in [("StockEntry", uid(&moved)), ("JournalEntry", uid(&je))] {
        let page = t.k.render_print(&t.ctx, entity, id).await.unwrap();
        lacks(&page, entity, &["<h2></h2>", "<dd></dd>", "dir=\"auto\"></div>"]);
    }
}

async fn account_in(t: &T, name: &str, company: &str) -> String {
    t.find_one("Account", json!({"account_name": name, "company": company})).await["id"].as_str().unwrap().to_string()
}
