-- Data policy (docs/DECISIONS.md, D4): only synthetic or already public data may enter the
-- platform, because the owner of the compute machine sees what it processes. A project records the
-- explicit acceptance of this policy; without it, there is no project.
ALTER TABLE projects ADD COLUMN data_policy text NOT NULL DEFAULT '';
