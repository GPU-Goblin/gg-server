-- Withdrawn artifacts (INCIDENTS.md, "harmful or mistaken artifact"). The bytes are deleted from
-- the object store; the row stays, so the hash is remembered: it is never served, announced or
-- accepted again, and the audit trail keeps who withdrew it and why.
ALTER TABLE artifacts
    ADD COLUMN withdrawn_at     timestamptz,
    ADD COLUMN withdrawn_reason text NOT NULL DEFAULT '';
