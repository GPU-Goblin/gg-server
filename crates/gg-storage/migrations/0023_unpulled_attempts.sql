-- An attempt closed because its job's lab node never got its outputs (ADR-0041): its worker is not
-- given the same task again, as after a rejection (it may be offline, or serve wrong bytes).
ALTER TABLE attempts ADD COLUMN outputs_not_pulled boolean NOT NULL DEFAULT false;
