-- fin_subscription_change: each change of a subscription's plans from the day it takes effect, with
-- its monthly recurring revenue (in the subscription's currency) before and after, so that MRR is
-- read as it stood on any day. A change dated on or before a later one's day replaces it.

create table fin_subscription_change (
  tenant_id uuid not null references tenant(id) on delete cascade,
  subscription_id uuid not null,
  effective date not null,
  mrr_before numeric not null,
  mrr_after numeric not null,
  created_at timestamptz not null default clock_timestamp(),
  foreign key (tenant_id, subscription_id) references e_subscription (tenant_id, id) on delete cascade
);
create index fin_subscription_change_of on fin_subscription_change (tenant_id, subscription_id, effective, created_at);

alter table fin_subscription_change enable row level security;
alter table fin_subscription_change force row level security;
create policy tenant_isolation on fin_subscription_change using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
