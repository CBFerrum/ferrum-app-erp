-- Where stock physically is. Not a second balance: `wms_position` changes only with a row in the
-- append-only `wms_move`, and in a managed warehouse the company's own positions always add up to
-- ERP's `stock_balance`, checked by the database at the end of every transaction.
create table wms_position (
  tenant_id uuid not null,
  id uuid not null,
  warehouse_id uuid not null,
  bin_id uuid not null,
  item_id uuid not null,
  -- A customer's stock, held for them: off this company's books, so outside ERP's balance.
  owner_id uuid,
  lot text not null default '',
  serial text not null default '',
  unit_id uuid,
  status text not null default 'Available' check (status in ('Available', 'Inspection', 'Quarantine', 'Hold', 'Damaged')),
  expiry date,
  received_on date not null default current_date,
  qty numeric(28,9) not null default 0 check (qty >= 0),
  -- Promised to open tasks. Only stock in place can be allocated.
  allocated numeric(28,9) not null default 0 check (allocated >= 0 and allocated <= qty),
  primary key (tenant_id, id)
);
create unique index wms_position_key on wms_position (tenant_id, bin_id, item_id, coalesce(owner_id, '00000000-0000-0000-0000-000000000000'), lot, serial, coalesce(unit_id, '00000000-0000-0000-0000-000000000000'), status);
create index wms_position_item on wms_position (tenant_id, warehouse_id, item_id) where qty > 0;
create index wms_position_bin on wms_position (tenant_id, bin_id) where qty > 0;
create index wms_position_lot on wms_position (tenant_id, item_id, lot) where lot <> '' and qty > 0;

-- Every physical change, for ever. `operation` makes a change idempotent: a fact a site repeats, or a
-- stock move ERP posted, lands once.
create table wms_move (
  tenant_id uuid not null,
  seq bigint generated always as identity,
  at timestamptz not null default now(),
  kind text not null check (kind in ('Receive', 'Issue', 'Putaway', 'Pick', 'Replenish', 'Move', 'Status', 'Adjust', 'Count', 'Return', 'Recover', 'Opening')),
  warehouse_id uuid not null,
  item_id uuid not null,
  qty numeric(28,9) not null check (qty > 0),
  from_bin uuid,
  to_bin uuid,
  owner_id uuid,
  lot text not null default '',
  serial text not null default '',
  from_unit uuid,
  to_unit uuid,
  from_status text,
  to_status text,
  task_id uuid,
  source_entity text,
  source_id uuid,
  actor uuid,
  reason text,
  operation text,
  primary key (tenant_id, seq)
);
create unique index wms_move_operation on wms_move (tenant_id, operation) where operation is not null;
create index wms_move_item on wms_move (tenant_id, item_id, seq desc);
create index wms_move_lot on wms_move (tenant_id, item_id, lot) where lot <> '';
create trigger wms_move_append_only before update or delete on wms_move for each row execute function ferrum_forbid_change();
create trigger wms_move_no_truncate before truncate on wms_move for each statement execute function ferrum_forbid_change();

-- Scans already taken, so a scanner can repeat itself after a lost answer.
create table wms_scan (
  tenant_id uuid not null,
  id uuid not null,
  at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- A customer's stock, bin-days, for its storage charge.
create table wms_storage_day (
  tenant_id uuid not null,
  day date not null,
  customer_id uuid not null,
  bins int not null,
  primary key (tenant_id, day, customer_id)
);

-- The contract between positions and ERP. Fires at commit, so a document may post stock and move
-- positions in either order, but never leave them apart.
create function wms_check_balance(p_tenant uuid, p_item uuid, p_warehouse uuid) returns void language plpgsql as $$
declare
  managed boolean;
  on_books numeric;
  in_place numeric;
begin
  select exists (select 1 from e_managed_warehouse m where m.tenant_id = p_tenant and m.warehouse = p_warehouse and m.status = 'Managed') into managed;
  if not managed then
    return;
  end if;
  select coalesce(sum(qty), 0) into on_books from stock_balance where tenant_id = p_tenant and item_id = p_item and warehouse_id = p_warehouse;
  select coalesce(sum(qty), 0) into in_place from wms_position where tenant_id = p_tenant and item_id = p_item and warehouse_id = p_warehouse and owner_id is null;
  if on_books <> in_place then
    raise exception 'stock and positions differ for this item: % on the books, % in place', on_books, in_place
      using errcode = 'P0001', hint = 'A managed warehouse moves stock through its tasks';
  end if;
end $$;

create function wms_balance_changed() returns trigger language plpgsql as $$
begin
  perform wms_check_balance(new.tenant_id, new.item_id, new.warehouse_id);
  return null;
end $$;

create constraint trigger wms_stock_matches after insert or update on stock_balance
  deferrable initially deferred for each row execute function wms_balance_changed();
create constraint trigger wms_position_matches after insert or update on wms_position
  deferrable initially deferred for each row execute function wms_balance_changed();

do $$
declare t text;
begin
  foreach t in array array['wms_position', 'wms_move', 'wms_scan', 'wms_storage_day'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
