-- Spot checks and probation (ADR-0042, owner's decision of 2026-09-26): a sample of the tasks, and
-- every task of an account still in probation, get a third computation from the start, whatever
-- the first two say. A pair that agrees, contradicted by that check, is suspected of collusion.
ALTER TABLE tasks ADD COLUMN spot_check boolean NOT NULL DEFAULT false;
