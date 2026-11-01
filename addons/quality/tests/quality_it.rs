//! A hold stops stock wherever a document would move it, and only the right person lifts it, against
//! a real PostgreSQL (`FERRUM_QUALITY_TEST_DATABASE_URL`; reset first).

use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, SiteRequest};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

async fn post(k: &Arc<Kernel>, ctx: &Ctx, path: &str, body: Value) -> Result<Value, Error> {
    let site = k.hooks.site("quality").unwrap().clone();
    let req = SiteRequest { tenant: ctx.tenant, user: Some(ctx.clone()), method: "POST".into(), path: path.into(), query: Map::new(), headers: Map::new(), body: body.to_string().into_bytes(), client_ip: "::1".into() };
    site.handle(k, req).await.map(|r| serde_json::from_slice(&r.body).unwrap())
}

async fn find(k: &Kernel, admin: &Ctx, entity: &str, filters: Value) -> Doc {
    let q = ListQuery { filters: parse_filters(&filters).unwrap(), ..Default::default() };
    k.list(admin, entity, &q).await.unwrap().rows.into_iter().next().unwrap()
}

#[test]
fn a_hold_stops_stock_and_only_the_right_person_lifts_it() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_QUALITY_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_quality_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().unwrap();
    packages.extend(ferrum_app_erp::packages().unwrap());
    packages.extend(ferrum_stock_ops::packages().unwrap());
    packages.extend(ferrum_quality::packages().unwrap());
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    ferrum_quality::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.unwrap();
    ferrum_app_erp::maintenance(&k).await.unwrap();
    let tenant = k
        .create_tenant(NewTenant { slug: "qa".into(), name: "QA Co".into(), admin_email: "admin@qa.test".into(), admin_password: "Correct-Horse-9".into(), admin_name: None, packages: vec!["quality".into()], settings: Some(json!({"fiscal_year_start_month": 4})) })
        .await
        .unwrap();
    let session = k.login("qa", "admin@qa.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let person = async |email: &str, roles: &[&str]| {
        let id = k.create_user(&admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.unwrap();
        k.load_ctx(tenant, id, None, "it".into()).await.unwrap()
    };
    let inr = find(&k, &admin, "Currency", json!({"code": "INR"})).await;
    let company = uid(&k.insert(&admin, "Company", obj(json!({"company_name": "QA Co", "abbr": "QAC", "default_currency": inr["id"]}))).await.unwrap());
    let wh = uid(&find(&k, &admin, "Warehouse", json!({"warehouse_name": "Stores", "company": company.to_string()})).await);
    let group = find(&k, &admin, "ItemGroup", json!({"group_name": "Products"})).await;
    let nos = find(&k, &admin, "Uom", json!({"uom_name": "Nos"})).await;
    let item = uid(&k.insert(&admin, "Item", obj(json!({"item_code": "PASTE", "item_name": "paste", "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": true, "has_batch_no": true}))).await.unwrap());
    k.insert(&admin, "Batch", obj(json!({"batch_id": "L1", "item": item}))).await.unwrap();
    k.insert(&admin, "Batch", obj(json!({"batch_id": "L2", "item": item}))).await.unwrap();
    let opening = k.insert(&admin, "StockEntry", obj(json!({"company": company, "purpose": "Material Receipt", "items": [
        {"item": item, "qty": 10, "basic_rate": 2, "target_warehouse": wh, "batch_no": "L1"}, {"item": item, "qty": 10, "basic_rate": 2, "target_warehouse": wh, "batch_no": "L2"}]}))).await.unwrap();
    k.submit(&admin, "StockEntry", uid(&opening), None).await.unwrap();

    let inspector = person("insp@qa.test", &["Quality User", "Stock User"]).await;
    let boss = person("boss@qa.test", &["Quality Manager"]).await;

    let issue = async |lot: &str| {
        let d = k.insert(&admin, "StockEntry", obj(json!({"company": company, "purpose": "Material Issue", "items": [{"item": item, "qty": 1, "source_warehouse": wh, "batch_no": lot}]}))).await.unwrap();
        k.submit(&admin, "StockEntry", uid(&d), None).await
    };

    // A hold on one lot stops that lot, and not the other.
    let hold = uid(&k.insert(&inspector, "QualityHold", obj(json!({"item": item, "lot": "L1", "reason": "Contamination suspected"}))).await.expect("placed"));
    let refused = issue("L1").await;
    assert!(matches!(refused, Err(Error::Validation(ref f)) if f[0].message.contains("quality hold")), "{refused:?}");
    issue("L2").await.expect("another lot is free");

    // Stock can still come in.
    let more = k.insert(&admin, "StockEntry", obj(json!({"company": company, "purpose": "Material Receipt", "items": [{"item": item, "qty": 1, "basic_rate": 2, "target_warehouse": wh, "batch_no": "L1"}]}))).await.unwrap();
    k.submit(&admin, "StockEntry", uid(&more), None).await.expect("a hold never stops a receipt");

    // Who may lift it.
    assert!(matches!(post(&k, &inspector, &format!("/holds/{hold}/release"), json!({"note": "looks fine"})).await, Err(Error::Forbidden(_))), "not without the role");
    assert!(post(&k, &boss, &format!("/holds/{hold}/release"), json!({"note": " "})).await.is_err(), "not without a reason");
    let own = uid(&k.insert(&boss, "QualityHold", obj(json!({"item": item, "lot": "L2", "reason": "Mine"}))).await.unwrap());
    assert!(matches!(post(&k, &boss, &format!("/holds/{own}/release"), json!({"note": "fine"})).await, Err(Error::Forbidden(_))), "not by who placed it, even a manager");
    assert!(k.update(&admin, "QualityHold", hold, obj(json!({"status": "Released"})), None).await.is_err() || k.get(&admin, "QualityHold", hold, false).await.unwrap()["status"] == json!("Active"), "and not by editing the record");
    post(&k, &boss, &format!("/holds/{hold}/release"), json!({"note": "Lab report 114 clear"})).await.expect("released");
    issue("L1").await.expect("usable again");
    assert_eq!(k.get(&admin, "QualityHold", hold, false).await.unwrap()["released_by"], json!("boss@qa.test"));

    // A disposition is decided once, by a manager, with a reason.
    let ncr = uid(&k.insert(&inspector, "Nonconformance", obj(json!({"title": "Lid seal", "severity": "Major", "item": item}))).await.unwrap());
    assert!(post(&k, &inspector, &format!("/ncr/{ncr}/disposition"), json!({"disposition": "Scrap", "note": "x"})).await.is_err());
    post(&k, &boss, &format!("/ncr/{ncr}/disposition"), json!({"disposition": "Scrap", "note": "Seal failed"})).await.expect("decided");
    assert!(post(&k, &boss, &format!("/ncr/{ncr}/disposition"), json!({"disposition": "Use As Is", "note": "changed my mind"})).await.is_err());

    // Corrective action closes on evidence, verified by someone else.
    let capa = uid(&k.insert(&boss, "Capa", obj(json!({"title": "Lid press", "owner_email": "boss@qa.test", "actions": [{"action": "Service the press"}]}))).await.unwrap());
    let tester = person("audit@qa.test", &["Quality Manager"]).await;
    assert!(post(&k, &boss, &format!("/capa/{capa}/verify"), json!({"note": "ok"})).await.is_err(), "not by its owner");
    assert!(post(&k, &tester, &format!("/capa/{capa}/verify"), json!({"note": "ok"})).await.is_err(), "not with actions open");
    let doc = k.get(&admin, "Capa", capa, false).await.unwrap();
    let mut actions = doc["actions"].clone();
    actions[0]["done_on"] = json!(chrono::Utc::now().date_naive().to_string());
    k.update(&boss, "Capa", capa, obj(json!({"actions": actions})), None).await.unwrap();
    post(&k, &tester, &format!("/capa/{capa}/verify"), json!({"note": "No rejects in 400 units"})).await.expect("verified");

    // An approved plan is frozen, and a new revision takes over.
    let template = k.insert(&admin, "QualityInspectionTemplate", obj(json!({"template_name": "Paste", "parameters": [{"parameter": "Viscosity", "numeric": true, "min_value": 1, "max_value": 2}]}))).await;
    let template = template.map(|t| uid(&t)).unwrap_or_else(|e| panic!("{e:?}"));
    let plan = uid(&k.insert(&boss, "InspectionPlan", obj(json!({"item": item, "revision": "A", "template": template}))).await.unwrap());
    post(&k, &boss, &format!("/plans/{plan}/approve"), Value::Null).await.expect("approved");
    assert!(k.update(&boss, "InspectionPlan", plan, obj(json!({"sample_size": 5})), None).await.is_err(), "an approved plan does not change");
    let next = uid(&k.insert(&boss, "InspectionPlan", obj(json!({"item": item, "revision": "B", "template": template, "sample_size": 5}))).await.unwrap());
    post(&k, &boss, &format!("/plans/{next}/approve"), Value::Null).await.unwrap();
    assert_eq!(k.get(&admin, "InspectionPlan", plan, false).await.unwrap()["status"], json!("Superseded"));
}
