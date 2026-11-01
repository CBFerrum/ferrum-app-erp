-- What happened on a run, for ever. A repeated event id lands once, so a terminal or a site may resend.
create table mes_event (
  tenant_id uuid not null,
  seq bigint generated always as identity,
  run_id uuid not null,
  event_id uuid not null,
  kind text not null check (kind in ('Release', 'Start', 'Pause', 'Resume', 'Consume', 'Report', 'Complete', 'Cancel')),
  at timestamptz not null,
  received_at timestamptz not null default now(),
  actor text,
  item_id uuid,
  lot text not null default '',
  serial text not null default '',
  qty numeric(28,9),
  scrap numeric(28,9),
  rework numeric(28,9),
  detail jsonb not null default 'null',
  primary key (tenant_id, seq)
);
create unique index mes_event_once on mes_event (tenant_id, event_id);
create index mes_event_run on mes_event (tenant_id, run_id, seq);
create trigger mes_event_append_only before update or delete on mes_event for each row execute function ferrum_forbid_change();
create trigger mes_event_no_truncate before truncate on mes_event for each statement execute function ferrum_forbid_change();

-- What went into what. Output lot empty means scrap: the material is accounted for, and made nothing.
create table mes_genealogy (
  tenant_id uuid not null,
  seq bigint generated always as identity,
  run_id uuid not null,
  input_item uuid not null,
  input_lot text not null default '',
  input_serial text not null default '',
  input_qty numeric(28,9) not null check (input_qty > 0),
  output_item uuid not null,
  output_lot text not null default '',
  at timestamptz not null default now(),
  primary key (tenant_id, seq)
);
create index mes_genealogy_forward on mes_genealogy (tenant_id, input_item, input_lot);
create index mes_genealogy_back on mes_genealogy (tenant_id, output_item, output_lot);
create trigger mes_genealogy_append_only before update or delete on mes_genealogy for each row execute function ferrum_forbid_change();

do $$
declare t text;
begin
  foreach t in array array['mes_event', 'mes_genealogy'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
