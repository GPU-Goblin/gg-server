-- A laboratory's lab node, shared (ADR-0041, A7). The researcher who registered a node grants other
-- researchers the use of it; a researcher allowed to use it attaches it to projects they are a
-- member of. The node acts only for the projects attached to it.
CREATE TABLE node_users (
    node_id       uuid NOT NULL REFERENCES lab_nodes(id),
    researcher_id uuid NOT NULL REFERENCES researchers(id),
    granted_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, researcher_id)
);

CREATE TABLE node_projects (
    node_id     uuid NOT NULL REFERENCES lab_nodes(id),
    project_id  uuid NOT NULL REFERENCES projects(id),
    attached_by uuid NOT NULL REFERENCES researchers(id),
    attached_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, project_id)
);
CREATE INDEX node_projects_project_idx ON node_projects (project_id);
