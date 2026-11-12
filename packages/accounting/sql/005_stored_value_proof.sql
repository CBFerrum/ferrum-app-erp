-- A card's code and PIN never come to rest (src/stored_value.rs, prove). The platform logs an
-- action's parameters and keeps a change's result as its receipt, so a code and PIN go only to the
-- kind's prove, which keeps their hashes here and answers a random proof: for one operation on one
-- kind, used once, gone after a few minutes. `effect_key`: the key the operation runs under, so a
-- retry under it is given the same proof and repeats its receipt.
create table stored_value_proof (
  tenant_id uuid not null,
  proof text not null,
  kind_id uuid not null,
  operation text not null,
  effect_key text,
  code_hash text not null,
  code_hint text not null,
  pin_hash text,
  made boolean not null default false,
  expires_at timestamptz not null,
  used_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, proof)
);
create unique index stored_value_proof_key on stored_value_proof (tenant_id, kind_id, operation, effect_key) where effect_key is not null;
alter table stored_value_proof enable row level security;
alter table stored_value_proof force row level security;
create policy tenant_isolation on stored_value_proof using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
