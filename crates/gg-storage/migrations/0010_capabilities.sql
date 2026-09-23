-- Capabilities and requirements (docs/ROADMAP.md, step C1). A task carries what it needs to run:
-- host memory, accelerator memory, minimum operator profile version. A worker announces its own on
-- every claim; the server offers it only what it can carry, instead of letting it fail and then
-- reassigning the task.
ALTER TABLE tasks
    ADD COLUMN required_ram_bytes bigint NOT NULL DEFAULT 0 CHECK (required_ram_bytes >= 0),
    ADD COLUMN required_device_bytes bigint NOT NULL DEFAULT 0 CHECK (required_device_bytes >= 0),
    ADD COLUMN required_profile integer NOT NULL DEFAULT 1 CHECK (required_profile >= 1);

ALTER TABLE workers
    ADD COLUMN announced_ram_bytes bigint,
    ADD COLUMN announced_device_bytes bigint,
    ADD COLUMN announced_profile integer,
    ADD COLUMN announced_engine text,
    ADD COLUMN announced_at timestamptz;
