-- What the upkeep told the accounts managers it could not do on a record (a recurring journal not made, an entry not
-- reversed on its day), one row a record. The upkeep runs about once a minute: a round that fails the same way on the
-- record as it stands tells nobody, and two rounds at once tell once (the second waits on the first's row and finds
-- it told). People are told again when the reason changes or the record's version moved.
create table upkeep_told (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  reason text not null,
  version int,
  told_at timestamptz not null default now(),
  primary key (tenant_id, entity, record_id)
);

alter table upkeep_told enable row level security;
alter table upkeep_told force row level security;
create policy tenant_isolation on upkeep_told using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
