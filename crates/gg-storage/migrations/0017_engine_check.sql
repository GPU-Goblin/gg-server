-- Engine check (ADR-0031): a result a GPU validator recomputed on the GPU is, at a configured rate,
-- recomputed again by a CPU validator. It catches an engine defect the GPU workers and the GPU
-- validator share, which no GPU-against-GPU comparison can see. It may be taken by the same
-- operator (it is a check of the engine, not an opinion against collusion), never by the same
-- validator.
ALTER TABLE verifications DROP CONSTRAINT verifications_kind_check;
ALTER TABLE verifications ADD CONSTRAINT verifications_kind_check
    CHECK (kind IN ('primary', 'audit', 'engine_check'));
