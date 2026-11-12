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
alter table stored_value_hold enable row level security;
alter table stored_value_hold force row level security;
create policy tenant_isolation on stored_value_hold using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
