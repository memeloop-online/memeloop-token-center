-- Coordination only: canonical/legacy roots remain the reference authority.
-- No TTL can release a pin or an ambiguous delete on behalf of its owner.
CREATE TABLE archive_gc_scopes (
    tenant_id TEXT NOT NULL,
    backend_identity TEXT NOT NULL CHECK (backend_identity = 'archive-all-backends'),
    root_epoch BIGINT NOT NULL DEFAULT 0 CHECK (root_epoch >= 0),
    phase TEXT NOT NULL DEFAULT 'open' CHECK (phase IN ('open','exclusive','delete_unknown')),
    gc_token TEXT,
    PRIMARY KEY (tenant_id, backend_identity),
    CHECK ((phase = 'open' AND gc_token IS NULL) OR (phase <> 'open' AND gc_token IS NOT NULL))
);
CREATE TABLE archive_gc_pins (
    tenant_id TEXT NOT NULL,
    backend_identity TEXT NOT NULL CHECK (backend_identity = 'archive-all-backends'),
    token TEXT NOT NULL,
    owner_kind TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('writer','reader')),
    active BIGINT NOT NULL DEFAULT 1 CHECK (active IN (0,1)),
    acquired_epoch BIGINT NOT NULL CHECK (acquired_epoch > 0),
    PRIMARY KEY (tenant_id, backend_identity, token),
    FOREIGN KEY (tenant_id, backend_identity) REFERENCES archive_gc_scopes (tenant_id, backend_identity)
);
