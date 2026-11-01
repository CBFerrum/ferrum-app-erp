//! Where stock may go, and where it should. A bin that fails any hard rule is refused whoever
//! asks: an operator's scan and the suggestion go through the same check.

use ferrum_kernel::{Error, Result, Tx};
use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Bin {
    pub id: Uuid,
    pub bin_code: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub frozen_by: Option<Uuid>,
    pub mixed_items: Option<bool>,
    pub max_weight_kg: Option<Decimal>,
    pub max_volume_l: Option<Decimal>,
    pub max_units: Option<Decimal>,
    pub hazard_classes: Option<String>,
    pub zone_kind: Option<String>,
    pub zone_available: Option<bool>,
    pub zone_temperature: Option<String>,
    pub warehouse: Uuid,
}

const BIN: &str = "select b.id, b.bin_code, b.kind, b.status, b.frozen_by, b.mixed_items, b.max_weight_kg, b.max_volume_l, b.max_units, b.hazard_classes, \
    z.kind as zone_kind, z.available as zone_available, z.temperature_class as zone_temperature, b.warehouse \
    from e_storage_bin b join e_storage_zone z on z.tenant_id = b.tenant_id and z.id = b.zone where b.tenant_id = $1";

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Goods {
    pub item_code: String,
    pub wms_weight_kg: Option<Decimal>,
    pub wms_volume_l: Option<Decimal>,
    pub wms_temperature_class: Option<String>,
    pub wms_hazard_class: Option<String>,
}

pub async fn bin(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Bin> {
    sqlx::query_as::<_, Bin>(sqlx::AssertSqlSafe(format!("{BIN} and b.id = $2"))).bind(tenant).bind(id).fetch_optional(&mut **tx).await?.ok_or_else(|| Error::NotFound("bin".into()))
}

pub async fn bin_by_code(tx: &mut Tx, tenant: Uuid, warehouse: Uuid, code: &str) -> Result<Bin> {
    sqlx::query_as::<_, Bin>(sqlx::AssertSqlSafe(format!("{BIN} and b.warehouse = $2 and b.bin_code = $3")))
        .bind(tenant)
        .bind(warehouse)
        .bind(code)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::invalid(format!("no bin {code} in this warehouse")))
}

pub async fn goods(tx: &mut Tx, tenant: Uuid, item: Uuid) -> Result<Goods> {
    sqlx::query_as("select item_code, wms_weight_kg, wms_volume_l, wms_temperature_class, wms_hazard_class from e_item where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(item)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("item".into()))
}

/// Why `goods` cannot go into `bin` as it stands, ignoring what is already there.
pub fn unfit(goods: &Goods, bin: &Bin) -> Option<String> {
    if bin.status.as_deref() == Some("Blocked") {
        return Some(format!("{} is blocked", bin.bin_code));
    }
    if bin.frozen_by.is_some() {
        return Some(format!("{} is being counted", bin.bin_code));
    }
    if bin.zone_available == Some(false) {
        return Some(format!("the zone of {} is not available", bin.bin_code));
    }
    let wanted = goods.wms_temperature_class.as_deref().unwrap_or("Ambient");
    if bin.zone_temperature.as_deref().unwrap_or("Ambient") != wanted {
        return Some(format!("{} needs a {} zone", goods.item_code, wanted.to_lowercase()));
    }
    if let Some(class) = goods.wms_hazard_class.as_deref().filter(|c| !c.is_empty()) {
        let allowed = bin.hazard_classes.as_deref().unwrap_or_default();
        if !allowed.split(',').any(|c| c.trim() == class) {
            return Some(format!("{} may not hold hazard class {class}", bin.bin_code));
        }
    }
    None
}

#[derive(sqlx::FromRow, Default)]
struct Load {
    units: Option<Decimal>,
    weight: Option<Decimal>,
    volume: Option<Decimal>,
    other_items: Option<i64>,
}

async fn load(tx: &mut Tx, tenant: Uuid, bin: Uuid, item: Uuid) -> Result<Load> {
    Ok(sqlx::query_as(
        "select sum(p.qty) as units, sum(p.qty * coalesce(i.wms_weight_kg, 0)) as weight, sum(p.qty * coalesce(i.wms_volume_l, 0)) as volume, \
                count(*) filter (where p.item_id <> $3) as other_items \
         from wms_position p join e_item i on i.tenant_id = p.tenant_id and i.id = p.item_id where p.tenant_id = $1 and p.bin_id = $2 and p.qty > 0",
    )
    .bind(tenant)
    .bind(bin)
    .bind(item)
    .fetch_one(&mut **tx)
    .await?)
}

/// Checks that `qty` of `item` fits in `bin` now. Refuses with what a person needs to know.
pub async fn check(tx: &mut Tx, tenant: Uuid, bin: &Bin, item: Uuid, qty: Decimal) -> Result<()> {
    let g = goods(tx, tenant, item).await?;
    if let Some(why) = unfit(&g, bin) {
        return Err(Error::invalid(why));
    }
    let l = load(tx, tenant, bin.id, item).await?;
    if bin.mixed_items == Some(false) && l.other_items.unwrap_or(0) > 0 {
        return Err(Error::invalid(format!("{} takes one item at a time", bin.bin_code)));
    }
    let over = |limit: Option<Decimal>, now: Option<Decimal>, more: Decimal| limit.is_some_and(|m| m > Decimal::ZERO && now.unwrap_or_default() + more > m);
    if over(bin.max_units, l.units, qty) {
        return Err(Error::invalid(format!("{} has no room for that many", bin.bin_code)));
    }
    if over(bin.max_weight_kg, l.weight, qty * g.wms_weight_kg.unwrap_or_default()) {
        return Err(Error::invalid(format!("that is over the weight limit of {}", bin.bin_code)));
    }
    if over(bin.max_volume_l, l.volume, qty * g.wms_volume_l.unwrap_or_default()) {
        return Err(Error::invalid(format!("that does not fit in {}", bin.bin_code)));
    }
    Ok(())
}

/// A bin for `qty` of `item`: one that already holds it first, then the emptiest storage bin that
/// fits. None when nothing does, and the task stays open for a person to decide.
pub async fn suggest(tx: &mut Tx, tenant: Uuid, warehouse: Uuid, item: Uuid, qty: Decimal) -> Result<Option<Uuid>> {
    let candidates: Vec<Bin> = sqlx::query_as::<_, Bin>(sqlx::AssertSqlSafe(format!(
        "{BIN} and b.warehouse = $2 and coalesce(b.status, 'Open') = 'Open' and b.frozen_by is null and z.kind in ('Storage', 'Picking') \
         order by (select count(*) from wms_position p where p.tenant_id = b.tenant_id and p.bin_id = b.id and p.item_id = $3 and p.qty > 0) desc, \
                  (select coalesce(sum(p.qty), 0) from wms_position p where p.tenant_id = b.tenant_id and p.bin_id = b.id) asc, b.pick_sequence, b.bin_code limit 200"
    )))
    .bind(tenant)
    .bind(warehouse)
    .bind(item)
    .fetch_all(&mut **tx)
    .await?;
    for b in candidates {
        if check(tx, tenant, &b, item, qty).await.is_ok() {
            return Ok(Some(b.id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin() -> Bin {
        Bin {
            id: Uuid::nil(),
            bin_code: "A-01".into(),
            kind: None,
            status: Some("Open".into()),
            frozen_by: None,
            mixed_items: Some(true),
            max_weight_kg: None,
            max_volume_l: None,
            max_units: None,
            hazard_classes: None,
            zone_kind: Some("Storage".into()),
            zone_available: Some(true),
            zone_temperature: Some("Ambient".into()),
            warehouse: Uuid::nil(),
        }
    }

    fn goods() -> Goods {
        Goods { item_code: "BOLT".into(), wms_weight_kg: None, wms_volume_l: None, wms_temperature_class: None, wms_hazard_class: None }
    }

    #[test]
    fn hard_rules_refuse_whoever_asks() {
        assert_eq!(unfit(&goods(), &bin()), None);
        assert!(unfit(&goods(), &Bin { status: Some("Blocked".into()), ..bin() }).is_some());
        assert!(unfit(&goods(), &Bin { frozen_by: Some(Uuid::nil()), ..bin() }).is_some(), "a bin being counted takes nothing");
        assert!(unfit(&goods(), &Bin { zone_available: Some(false), ..bin() }).is_some());
        let frozen = Goods { wms_temperature_class: Some("Frozen".into()), ..goods() };
        assert!(unfit(&frozen, &bin()).is_some());
        assert_eq!(unfit(&frozen, &Bin { zone_temperature: Some("Frozen".into()), ..bin() }), None);
    }

    #[test]
    fn hazard_classes_are_listed_exactly() {
        let acid = Goods { wms_hazard_class: Some("8".into()), ..goods() };
        assert!(unfit(&acid, &bin()).is_some(), "none allowed by default");
        assert_eq!(unfit(&acid, &Bin { hazard_classes: Some("3, 8".into()), ..bin() }), None);
        assert!(unfit(&acid, &Bin { hazard_classes: Some("80".into()), ..bin() }).is_some());
    }
}
