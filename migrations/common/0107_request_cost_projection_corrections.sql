-- Historical billing inputs remain immutable. Corrections apply only to the
-- operator-facing statistics projection and retain the exact evidence used by
-- each versioned repair so a replay is both idempotent and auditable.
CREATE TABLE request_cost_projection_corrections (
    correction_version TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_created_at BIGINT NOT NULL,
    evidence_kind TEXT NOT NULL CHECK (
        evidence_kind IN ('provider_reported', 'reservation_ceiling_without_usage')
    ),
    observed_status_code BIGINT NOT NULL,
    observed_error_code TEXT NOT NULL,
    observed_usage_basis TEXT,
    reservation_id TEXT NOT NULL,
    reservation_reserved_micros BIGINT NOT NULL CHECK (reservation_reserved_micros >= 0),
    reservation_actual_micros BIGINT,
    original_request_cost_micros BIGINT NOT NULL CHECK (original_request_cost_micros >= 0),
    original_fact_cost_micros BIGINT NOT NULL CHECK (original_fact_cost_micros >= 0),
    corrected_fact_cost_micros BIGINT NOT NULL CHECK (corrected_fact_cost_micros >= 0),
    applied_at BIGINT NOT NULL,
    PRIMARY KEY (correction_version, request_id)
);

CREATE INDEX request_cost_projection_corrections_time_idx
    ON request_cost_projection_corrections (request_created_at, request_id);
