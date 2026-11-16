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
  remarks text,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, account_id) references e_stored_value_account (tenant_id, id)
);
create index stored_value_entry_account on stored_value_entry (tenant_id, account_id, created_at);
create index stored_value_entry_source on stored_value_entry (tenant_id, source_entity, source_id);
create index stored_value_entry_op on stored_value_entry (tenant_id, op_id);
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

-- Stored value held for an order before it is invoiced (src/stored_value.rs, holds). A hold sets
-- part of an account's balance aside for one order: nothing is booked and the balance stays, but
-- nothing else may take what is held. It ends once: released (by hand or when the order is
-- cancelled), expired (by the upkeep, after the kind's hours), or redeemed (when the order is
-- invoiced, as a payment from the account); each ending is one update from 'held', so it happens
-- exactly once whoever races for it.
create table stored_value_hold (
  tenant_id uuid not null,
  id uuid not null,
  account_id uuid not null,
  order_entity text not null,
  order_id uuid not null,
  amount numeric(28,9) not null check (amount > 0),
  expires_at timestamptz not null,
  state text not null default 'held' check (state in ('held', 'released', 'expired', 'redeemed')),
  invoice_id uuid,
  payment_id uuid,
  created_by uuid,
  created_at timestamptz not null default now(),
  closed_at timestamptz,
  primary key (tenant_id, id),
  foreign key (tenant_id, account_id) references e_stored_value_account (tenant_id, id)
);
create index stored_value_hold_order on stored_value_hold (tenant_id, order_id) where state = 'held';
create index stored_value_hold_account on stored_value_hold (tenant_id, account_id) where state = 'held';
create index stored_value_hold_due on stored_value_hold (tenant_id, expires_at) where state = 'held';

-- A card's code (and PIN) typed on a sales invoice's line or a payment waits here for its document to be submitted
-- (src/stored_value.rs, `keep`): the code's hash, its last four characters and the PIN's hash, under a random handle.
-- The record keeps only the handle and the four characters (`handle:hint` in `stored_value_secret`), which
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

do $$
declare t text;
begin
  foreach t in array array['stored_value_entry', 'stored_value_attempt', 'stored_value_proof', 'stored_value_hold', 'stored_value_typed'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
