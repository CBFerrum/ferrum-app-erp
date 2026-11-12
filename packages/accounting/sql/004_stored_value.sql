-- Stored value's own ledger (src/stored_value.rs): every change to an account is one appended
-- entry, never updated or deleted. An account's balance is the sum of its entries' amounts (money,
-- or units) and its value the sum of their values (what the books owe, company currency); an
-- operation's entries share `op_id`, which is also the source of its own journal entry when it
-- posts one. A code is never stored: an account keeps sha-256 of it (with the workspace's id).

create table stored_value_entry (
  tenant_id uuid not null,
  id uuid not null,
  op_id uuid not null,
  account_id uuid not null,
  entry_type text not null check (entry_type in ('issue', 'top_up', 'redeem', 'refund', 'transfer', 'expire', 'breakage', 'adjust', 'reverse')),
  amount numeric(28,9) not null,
  value numeric(28,9) not null,
  posting_date date not null,
  source_entity text,
  source_id uuid,
  source_line text,
  against_id uuid,
  journal_entry_id uuid,
  reversal_of uuid,
  op_key text,
  op_digest text,
  remarks text,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, account_id) references e_stored_value_account (tenant_id, id)
);
create index stored_value_entry_account on stored_value_entry (tenant_id, account_id, created_at);
create index stored_value_entry_source on stored_value_entry (tenant_id, source_entity, source_id);
create unique index stored_value_entry_key on stored_value_entry (tenant_id, op_key) where op_key is not null;
create unique index stored_value_entry_reversal on stored_value_entry (tenant_id, reversal_of) where reversal_of is not null;
create unique index stored_value_account_code on e_stored_value_account (tenant_id, code_hash) where code_hash is not null;
create trigger stored_value_entry_append_only before update or delete on stored_value_entry
  for each row execute function ferrum_forbid_change();
create trigger stored_value_entry_no_truncate before truncate on stored_value_entry
  for each statement execute function ferrum_forbid_change();

-- No account goes below nothing, whatever wrote the entry.
create or replace function ferrum_check_stored_value() returns trigger
language plpgsql as $$
begin
  if (select sum(amount) from stored_value_entry where tenant_id = new.tenant_id and account_id = new.account_id) < 0 then
    raise exception 'ferrum: stored value account % would go below nothing', new.account_id using errcode = 'P0001';
  end if;
  return null;
end $$;
create constraint trigger stored_value_entry_not_negative after insert on stored_value_entry
  deferrable initially deferred for each row execute function ferrum_check_stored_value();

-- Codes and PINs that did not match, for refusing whoever keeps guessing.
create table stored_value_attempt (
  tenant_id uuid not null,
  account_id uuid,
  user_id uuid,
  at timestamptz not null default now()
);
create index stored_value_attempt_account on stored_value_attempt (tenant_id, account_id, at) where account_id is not null;
create index stored_value_attempt_user on stored_value_attempt (tenant_id, user_id, at);

do $$
declare t text;
begin
  foreach t in array array['stored_value_entry', 'stored_value_attempt'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
