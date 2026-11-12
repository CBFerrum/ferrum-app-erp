-- Accounting dimensions (FN-01): a line's values are kept in dims as {code: value}; lines written
-- before have '{}', which reports read as none. Reports filter with dims @> '{"code": "value"}',
-- served by this index; it holds only lines that carry a dimension, so on upgrade it starts empty.
create index if not exists journal_line_dims on journal_line using gin (dims jsonb_path_ops) where dims <> '{}'::jsonb;
