-- What a released run has set aside. A balance says what exists, not whose it is: without this, two
-- runs each find enough and neither has it. Gone when the run closes.
create table mes_reservation (
  tenant_id uuid not null,
  run_id uuid not null,
  item_id uuid not null,
  warehouse_id uuid not null,
  qty numeric(28,9) not null check (qty >= 0),
  primary key (tenant_id, run_id, item_id, warehouse_id)
);
create index mes_reservation_item on mes_reservation (tenant_id, item_id, warehouse_id);

alter table mes_reservation enable row level security;
alter table mes_reservation force row level security;
create policy tenant_isolation on mes_reservation using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
