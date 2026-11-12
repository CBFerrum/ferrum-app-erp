-- What ERP 0.20.0 left behind, and nothing else: the code, PIN and other card's code its stored value
-- operations took as parameters, in their own audit events only (each keeps its hash and place in the
-- chain and records the redaction, as erasing personal data does), and the codes made for new cards
-- in those operations' receipts.
select set_config('ferrum.system', 'on', true);
update audit_event a
   set diff = a.diff || (select jsonb_object_agg(f, '"[erased]"'::jsonb) from unnest(array['code', 'pin', 'to_code']) f where a.diff ? f and a.diff -> f <> 'null'::jsonb),
       redacted = coalesce(a.redacted, '[]'::jsonb) || jsonb_build_array(jsonb_build_object(
         'fields', (select jsonb_agg(f) from unnest(array['code', 'pin', 'to_code']) f where a.diff ? f and a.diff -> f <> 'null'::jsonb),
         'by', null, 'at', now(), 'reason', 'A stored value code or PIN taken as a parameter (ERP 0.20.0)'))
 where ((a.entity = 'StoredValueKind' and a.action in ('action:issue', 'action:redeem', 'action:refund', 'action:transfer'))
        or (a.entity = 'StoredValueAccount' and a.action = 'action:new_code'))
   and jsonb_typeof(a.diff) = 'object'
   and exists (select 1 from unnest(array['code', 'pin', 'to_code']) f where a.diff ? f and a.diff -> f <> 'null'::jsonb);
update logic_effect set result = result - 'code'
 where capability in ('accounting.stored_value_kind.issue', 'accounting.stored_value_kind.refund', 'accounting.stored_value_kind.transfer')
   and jsonb_typeof(result) = 'object' and result ? 'code';
select set_config('ferrum.system', 'off', true);
