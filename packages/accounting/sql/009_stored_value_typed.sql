-- A card's code (and PIN) typed on a sales invoice's line or a payment waits here for its document to be submitted
-- (src/stored_value.rs, `keep`): the code's hash, its last four characters and the PIN's hash, under a random handle.
-- The record keeps only the handle and the four characters (`handle:hint:held` in `stored_value_secret`), which
-- nobody who reads the record, an export or the audit trail can use; the submission that opens the card takes the row.
create table stored_value_typed (
  tenant_id uuid not null,
  handle text not null,
  code_hash text not null,
  code_hint text not null,
  pin_hash text,
  created_at timestamptz not null default now(),
  primary key (tenant_id, handle)
);
alter table stored_value_typed enable row level security;
alter table stored_value_typed force row level security;
create policy tenant_isolation on stored_value_typed using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
