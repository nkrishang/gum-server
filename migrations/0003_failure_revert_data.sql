-- `PaymentFactory.execute` passes on the Payment constructor's revert data, which says exactly why
-- a settlement failed (e.g. CallFailed(0, "Blacklistable: account is blacklisted")). gum-engine
-- reports it when its simulation of `execute` reverts. Stored raw (0x hex) and decoded when a
-- deposit is served, as `failure.revert`.
ALTER TABLE deposits ADD COLUMN failure_revert_data TEXT;
