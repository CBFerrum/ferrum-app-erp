-- Finance books (parallel accounting): a journal entry with no book is in every book; one with a book
-- is in that book alone, beside the entries every book shares. A book is the common ledger and its
-- own adjustments (another depreciation, another valuation), and each balances, because each entry
-- does and all its lines are in its book.
alter table journal_entry add column finance_book_id uuid;
alter table journal_entry add constraint journal_entry_finance_book foreign key (tenant_id, finance_book_id) references e_finance_book (tenant_id, id);
alter table journal_line add column finance_book_id uuid;
create index journal_line_book on journal_line (tenant_id, company_id, finance_book_id, account_id, posting_date) where finance_book_id is not null;

create or replace function ferrum_check_journal_book() returns trigger
language plpgsql as $$
begin
  if exists (select 1 from journal_line l where l.tenant_id = new.tenant_id and l.entry_id = new.id and l.posting_date = new.posting_date
             and l.finance_book_id is distinct from new.finance_book_id) then
    raise exception 'ferrum: journal entry % has lines in another book than its own', new.id using errcode = 'P0001';
  end if;
  return null;
end $$;
create constraint trigger journal_entry_one_book after insert on journal_entry
  deferrable initially deferred for each row execute function ferrum_check_journal_book();

-- What a report reads: the common ledger, and the book the reading transaction names
-- (`set_config('ferrum.finance_book', <id>, true)`). Naming none reads the common ledger alone, as
-- before books existed. Row-level security holds as for the table (security_invoker).
create view journal_line_in_book with (security_invoker = true) as
  select * from journal_line
  where finance_book_id is null or finance_book_id = nullif(current_setting('ferrum.finance_book', true), '')::uuid;
