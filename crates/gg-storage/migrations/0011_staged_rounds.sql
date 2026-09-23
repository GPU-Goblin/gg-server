-- Staged execution (docs/ROADMAP.md, P11 D2). A round advances in waves: forward passes of each
-- stage, then backward passes in reverse order. `job_id` names the current wave; `stage_state`
-- keeps what earlier waves produced (micro-batch rows, activations, received gradients, parameter
-- gradients), per micro-batch, to build the next tasks.
ALTER TABLE collective_rounds ADD COLUMN stage_state jsonb;

-- Compute kinds a worker announced on its last claim: for staged execution, the plan counts only
-- workers that accept `graph.stage.v1`.
ALTER TABLE workers ADD COLUMN announced_kinds text[];
