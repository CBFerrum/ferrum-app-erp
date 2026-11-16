-- A posting takes its period's row shared as it checks it. Closing the period (an update of that row)
-- then waits for the postings already being made into it, and a posting that comes while a close is
-- being made waits for the close and finds the period closed. Read without a lock, a posting whose
-- check passed before a close committed still landed in the closed period.
create or replace function ferrum_journal_entry_guard() returns trigger
language plpgsql as $$
declare closed boolean;
begin
  select p.status = 'closed' into closed from accounting_period p
   where p.tenant_id = new.tenant_id and p.company_id = new.company_id
     and new.posting_date between p.period_start and p.period_end
   for share;
  if closed then
    raise exception 'ferrum: the accounting period containing % is closed', new.posting_date using errcode = 'P0001';
  end if;
  return new;
end $$;
