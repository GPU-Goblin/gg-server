-- Lease length chosen per attempt from the heartbeat rhythm the worker announces (several missed
-- heartbeats), instead of one length for every worker: a worker that vanishes is replaced after
-- seconds, not minutes. NULL: the server's configured length.
ALTER TABLE leases ADD COLUMN renew_secs integer CHECK (renew_secs IS NULL OR renew_secs > 0);
