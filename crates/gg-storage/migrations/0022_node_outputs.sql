-- Outputs pulled by a lab node (ADR-0041, A4). A job whose `output_holder` is set keeps its large
-- outputs on the workers that produced them; the node pulls them, checks them and becomes their
-- holder. Until then they are pending (`pending_outputs`), and nothing is verified.
ALTER TABLE jobs ADD COLUMN output_holder uuid REFERENCES lab_nodes(id);
