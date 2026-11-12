//! What the finance tests share: a kernel with ERP on a database of their own (reset first), a
//! workspace and a company with the standard chart, and short ways to write and read records.
#![allow(dead_code)]

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

/// Files kept in memory: what documents generated are stored in.
#[derive(Default)]
pub struct MemFiles(std::sync::Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| ferrum_kernel::Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

pub fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

pub fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

pub fn dec(v: &Value) -> Decimal {
    match v {
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap(),
        _ => Decimal::from_str(v.as_str().unwrap_or("0")).unwrap(),
    }
}

pub fn run(f: impl std::future::Future<Output = ()> + Send + 'static) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f).await.unwrap() });
}

pub struct T {
    pub k: Arc<Kernel>,
    pub ctx: Ctx,
    pub company: String,
}

impl T {
    /// Boots on `url`, with ERP and `extra` registered, a workspace with ERP's packages and the
    /// extra ones, and a company in INR with the standard chart.
    pub async fn boot(url: String, extra: impl FnOnce(&mut Vec<ferrum_meta::Package>, &mut HookRegistry)) -> T {
        T::boot_with(url, KernelSettings::default(), extra).await
    }

    /// [`T::boot`] with the workspace also given manufacturing.
    pub async fn boot_manufacturing(url: String) -> T {
        T::boot_inner(url, KernelSettings::default(), |_, _| {}, true).await
    }

    /// [`T::boot`] with the kernel's settings given (a secret key, private addresses allowed).
    pub async fn boot_with(url: String, settings: KernelSettings, extra: impl FnOnce(&mut Vec<ferrum_meta::Package>, &mut HookRegistry)) -> T {
        T::boot_inner(url, settings, extra, false).await
    }

    async fn boot_inner(
        url: String,
        settings: KernelSettings,
        extra: impl FnOnce(&mut Vec<ferrum_meta::Package>, &mut HookRegistry),
        manufacturing: bool,
    ) -> T {
        let pool = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&pool.pool).await.expect("reset");
        let mut packages = ferrum_domain::packages().expect("the shared packages");
        packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        let mut hooks = HookRegistry::default();
        ferrum_domain::register(&mut hooks);
        ferrum_app_erp::register(&mut hooks);
        extra(&mut packages, &mut hooks);
        for source in ferrum_app_erp::reports() {
            ferrum_domain::reports::add_source(source);
        }
        let names: Vec<String> = packages
            .iter()
            .map(|p| p.name().to_string())
            .filter(|n| !["contacts", "project_records", "parties", "products"].contains(&n.as_str()) && (manufacturing || n != "manufacturing"))
            .collect();
        let k: Arc<Kernel> = Kernel::boot(pool, Catalog::from_packages(packages).unwrap(), hooks, settings, None).await.expect("boot");
        k.set_file_store(Arc::new(MemFiles::default()));
        ferrum_domain::maintenance(&k).await.expect("maintenance");
        ferrum_app_erp::maintenance(&k).await.expect("partitions");
        let tenant = k
            .create_tenant(NewTenant {
                slug: "fin".into(),
                name: "Finance Co".into(),
                admin_email: "admin@fin.test".into(),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: names,
                settings: None,
            })
            .await
            .expect("tenant");
        let session = k.login("fin", "admin@fin.test", "Correct-Horse-9").await.unwrap();
        let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
        let mut t = T { k, ctx, company: String::new() };
        let inr = t.find_one("Currency", json!({"code": "INR"})).await;
        t.company = uid(&t.insert("Company", json!({"company_name": "Finance Co", "abbr": "FC", "default_currency": inr["id"]})).await).to_string();
        t
    }

    pub async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    pub async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    /// Inserts and submits; the error's text when refused.
    pub async fn try_post(&self, entity: &str, v: Value) -> Result<Doc, String> {
        let d = self.k.insert(&self.ctx, entity, obj(v)).await.map_err(|e| format!("{e:?}"))?;
        self.k.submit(&self.ctx, entity, uid(&d), None).await.map_err(|e| format!("{e:?}"))
    }
    pub async fn post(&self, entity: &str, v: Value) -> Doc {
        self.try_post(entity, v).await.unwrap_or_else(|e| panic!("post {entity}: {e}"))
    }
    pub async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    pub async fn action(&self, entity: &str, id: Uuid, name: &str, params: Value) -> Result<Value, String> {
        self.k.run_action(&self.ctx, entity, id, name, params).await.map_err(|e| format!("{e:?}"))
    }
    pub async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let rows = self.find(entity, filters.clone()).await;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    pub async fn find(&self, entity: &str, filters: Value) -> Vec<Doc> {
        self.k
            .list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() })
            .await
            .unwrap()
            .rows
    }
    pub async fn account(&self, name: &str) -> String {
        self.find_one("Account", json!({"account_name": name, "company": self.company})).await["id"].as_str().unwrap().to_string()
    }
    pub async fn report(&self, name: &str, params: &[(&str, &str)]) -> Value {
        let mut p: HashMap<String, String> = params.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        p.insert("company".into(), self.company.clone());
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    pub async fn sql<O: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin>(&self, q: &str, id: Uuid) -> Vec<O> {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let rows = sqlx::query_as(sqlx::AssertSqlSafe(q.to_string())).bind(id).fetch_all(&mut *tx).await.unwrap();
        tx.rollback().await.unwrap();
        rows
    }
}

/// Every warning or error the process logs, kept: a test that says "nothing was logged" reads it. Install once
/// with [`Warnings::install`] in a test binary that runs one scenario.
#[derive(Clone, Default)]
pub struct Warnings(pub Arc<std::sync::Mutex<Vec<String>>>);

struct Said(String);

impl tracing::field::Visit for Said {
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={v:?}", f.name()));
    }
}

impl tracing::Subscriber for Warnings {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        *m.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, e: &tracing::Event<'_>) {
        let mut said = Said(e.metadata().target().to_string());
        e.record(&mut said);
        self.0.lock().unwrap().push(said.0);
    }
}

impl Warnings {
    pub fn install() -> Warnings {
        let w = Warnings::default();
        tracing::subscriber::set_global_default(w.clone()).expect("no other subscriber");
        w
    }
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}
