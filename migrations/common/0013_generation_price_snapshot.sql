ALTER TABLE generation_jobs ADD COLUMN billing_unit_snapshot TEXT;
ALTER TABLE generation_jobs ADD COLUMN micros_per_unit_snapshot BIGINT;

UPDATE generation_jobs
SET billing_unit_snapshot = COALESCE(
        (SELECT p.billing_unit
         FROM usage_reservations r
         JOIN generation_prices p ON p.id = r.price_id
         WHERE r.id = generation_jobs.reservation_id),
        'job'
    ),
    micros_per_unit_snapshot = COALESCE(
        (SELECT p.micros_per_unit
         FROM usage_reservations r
         JOIN generation_prices p ON p.id = r.price_id
         WHERE r.id = generation_jobs.reservation_id),
        0
    );
