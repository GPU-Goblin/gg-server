-- Training runs stepped by a lab node (ADR-0041, A6): the node initializes the weights, averages the
-- accepted gradients, applies the optimizer step and holds every checkpoint; the server schedules.
ALTER TABLE collective_runs ADD COLUMN holder_node uuid REFERENCES lab_nodes(id);
-- The first bytes of an artifact a node holds, as the node declared them: what the server reads of
-- a dataset (its shape, in the tensor header) without holding the bytes.
ALTER TABLE artifacts ADD COLUMN node_header bytea CHECK (octet_length(node_header) <= 64);
