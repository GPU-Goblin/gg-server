-- Artifacts are content-addressed: two projects uploading identical bytes (or two collective runs
-- publishing identical zero biases) share one row. Read access is therefore a relation, not a column:
-- a project may read an artifact it uploaded or that was materialized for it, whoever registered the
-- hash first. `artifacts.project_id` keeps the first registrant for accounting only.

CREATE TABLE artifact_project_access (
    hash        bytea NOT NULL REFERENCES artifacts(hash),
    project_id  uuid NOT NULL REFERENCES projects(id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (hash, project_id)
);

INSERT INTO artifact_project_access (hash, project_id, created_at)
SELECT hash, project_id, created_at FROM artifacts WHERE project_id IS NOT NULL
ON CONFLICT DO NOTHING;
