-- A researcher's request for access, made from the account on the public site: who they are,
-- their laboratory and what they want to compute, for the operator who decides the admission.
ALTER TABLE researchers
    ADD COLUMN lab          text NOT NULL DEFAULT '' CHECK (length(lab) <= 200),
    ADD COLUMN purpose      text NOT NULL DEFAULT '' CHECK (length(purpose) <= 2000),
    ADD COLUMN requested_at timestamptz;
