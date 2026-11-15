-- Before ERP 0.27.0 a typed code's hash and its PIN's rested on the record itself (`hash:hint:pin`). A draft's move
-- to stored_value_typed, the record keeping a handle in their place, so the draft still opens its card with its code
-- and PIN; a submitted or cancelled document used them already, and keeps nothing. Running it again changes nothing.
select set_config('ferrum.system', 'on', true);
with typed as (
  select l.tenant_id, l.id, replace(gen_random_uuid()::text, '-', '') || replace(gen_random_uuid()::text, '-', '') as handle,
         split_part(l.stored_value_secret, ':', 1) as code_hash, split_part(l.stored_value_secret, ':', 2) as code_hint,
         nullif(split_part(l.stored_value_secret, ':', 3), '') as pin_hash
    from e_sales_invoice__items l join e_sales_invoice i on i.tenant_id = l.tenant_id and i.id = l.parent_id
   where i.docstatus = 0 and l.stored_value_secret like '%:%' and split_part(l.stored_value_secret, ':', 3) <> 'held'
), moved as (
  insert into stored_value_typed (tenant_id, handle, code_hash, code_hint, pin_hash) select tenant_id, handle, code_hash, code_hint, pin_hash from typed
)
update e_sales_invoice__items l set stored_value_secret = t.handle || ':' || t.code_hint || ':held' from typed t where l.tenant_id = t.tenant_id and l.id = t.id;
update e_sales_invoice__items l set stored_value_secret = null from e_sales_invoice i
 where i.tenant_id = l.tenant_id and i.id = l.parent_id and i.docstatus <> 0 and l.stored_value_secret is not null;
with typed as (
  select p.tenant_id, p.id, replace(gen_random_uuid()::text, '-', '') || replace(gen_random_uuid()::text, '-', '') as handle,
         split_part(p.stored_value_secret, ':', 1) as code_hash, split_part(p.stored_value_secret, ':', 2) as code_hint,
         nullif(split_part(p.stored_value_secret, ':', 3), '') as pin_hash
    from e_payment_entry p
   where p.docstatus = 0 and p.stored_value_secret like '%:%' and split_part(p.stored_value_secret, ':', 3) <> 'held'
), moved as (
  insert into stored_value_typed (tenant_id, handle, code_hash, code_hint, pin_hash) select tenant_id, handle, code_hash, code_hint, pin_hash from typed
)
update e_payment_entry p set stored_value_secret = t.handle || ':' || t.code_hint || ':held' from typed t where p.tenant_id = t.tenant_id and p.id = t.id;
update e_payment_entry set stored_value_secret = null where docstatus <> 0 and stored_value_secret is not null;
select set_config('ferrum.system', 'off', true);
