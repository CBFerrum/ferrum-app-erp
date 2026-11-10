-- Returns made before ERP 0.15.0 did not say which line of their original they gave back, and
-- originals did not keep what came back. Each old return line is tied to the first line of its
-- original with its item, and each original line gets what its submitted returns took back (in
-- the line's own unit) and, on invoices, what they gave back before tax. Every workspace's rows.
select set_config('ferrum.system', 'on', true);

update e_purchase_receipt__items r set return_line = (
  select o.id::text from e_purchase_receipt__items o where o.tenant_id = r.tenant_id and o.parent_id = p.return_against and o.item = r.item order by o.idx limit 1)
from e_purchase_receipt p
where p.tenant_id = r.tenant_id and p.id = r.parent_id and p.is_return and p.return_against is not null and r.return_line is null;

update e_purchase_receipt__items o set returned_qty = x.stock / coalesce(nullif(o.conversion_factor, 0), 1)
from (select r.tenant_id, r.return_line, sum(abs(coalesce(r.stock_qty, r.qty))) as stock from e_purchase_receipt__items r
      join e_purchase_receipt p on p.tenant_id = r.tenant_id and p.id = r.parent_id
      where p.is_return and p.docstatus = 1 and r.return_line is not null group by 1, 2) x
where x.tenant_id = o.tenant_id and o.id::text = x.return_line;

update e_purchase_invoice__items r set return_line = (
  select o.id::text from e_purchase_invoice__items o where o.tenant_id = r.tenant_id and o.parent_id = p.return_against and o.item = r.item order by o.idx limit 1)
from e_purchase_invoice p
where p.tenant_id = r.tenant_id and p.id = r.parent_id and p.is_return and p.return_against is not null and r.return_line is null;

update e_purchase_invoice__items o set returned_qty = x.stock / coalesce(nullif(o.conversion_factor, 0), 1)
from (select r.tenant_id, r.return_line, sum(abs(coalesce(r.stock_qty, r.qty))) as stock from e_purchase_invoice__items r
      join e_purchase_invoice p on p.tenant_id = r.tenant_id and p.id = r.parent_id
      where p.is_return and p.docstatus = 1 and not coalesce(p.price_only, false) and r.return_line is not null group by 1, 2) x
where x.tenant_id = o.tenant_id and o.id::text = x.return_line;

update e_purchase_invoice__items o set credited_amount = x.amount
from (select r.tenant_id, r.return_line, sum(abs(r.net_amount)) as amount from e_purchase_invoice__items r
      join e_purchase_invoice p on p.tenant_id = r.tenant_id and p.id = r.parent_id
      where p.is_return and p.docstatus = 1 and r.return_line is not null group by 1, 2) x
where x.tenant_id = o.tenant_id and o.id::text = x.return_line;
