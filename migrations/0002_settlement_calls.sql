-- Settlement calls (gum-contracts PR #1): a payment no longer pays a fixed receiver; it runs an
-- ordered list of committed calls, [{"target": "0x…", "data": "0x…"}]. The payment address is
-- derived from them and `PaymentFactory.execute` takes them, so they are stored exactly as
-- committed. A plain deposit's list is one `token.transfer(receiver, amount)`.
--
-- Rows created before this migration belong to the previous contract generation (CREATE3 with a
-- fixed receiver), which this server no longer executes; they were all terminal at the cutover
-- and keep an empty list.
ALTER TABLE deposits ADD COLUMN calls JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE deposits ALTER COLUMN calls DROP DEFAULT;
