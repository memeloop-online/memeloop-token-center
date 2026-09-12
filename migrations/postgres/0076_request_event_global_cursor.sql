-- PostgreSQL has had this exact partitioned-table cursor index since v24.
-- Keep an explicit backend migration at v76 so both registries advance in
-- lockstep without attempting to recreate or silently rename that index.
SELECT 1;
