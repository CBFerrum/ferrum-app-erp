-- What a production run asked to have at the line, and how much of what is there is that run's. A
-- quantity at the line is not a promise to anyone, so without this two runs would both count it.
create table wms_supply (
  tenant_id uuid not null,
  run_id uuid not null,
  warehouse_id uuid not null,
  item_id uuid not null,
  need numeric(28,9) not null check (need >= 0),
  at_line numeric(28,9) not null default 0 check (at_line >= 0),
  closed_at timestamptz,
  primary key (tenant_id, run_id, warehouse_id, item_id)
);
create index wms_supply_open on wms_supply (tenant_id, warehouse_id, item_id) where closed_at is null;

alter table wms_supply enable row level security;
alter table wms_supply force row level security;
create policy tenant_isolation on wms_supply using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
