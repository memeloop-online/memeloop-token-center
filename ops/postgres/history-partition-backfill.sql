-- PostgreSQL-only operational support for online history repartitioning.
--
-- This file is deliberately not an application schema migration.  It installs
-- a restartable operator procedure and its progress table.  Run it through
-- ops/backfill-postgres-history-partitions.sh, which defaults to a read-only
-- dry run and holds a session advisory lock around each CALL.

CREATE TABLE IF NOT EXISTS public.mtc_history_partition_backfill_state (
    table_name text NOT NULL,
    day_start date NOT NULL,
    status text NOT NULL CHECK (status IN ('copying', 'validating', 'complete')),
    source_rows bigint NOT NULL DEFAULT 0,
    staged_rows bigint NOT NULL DEFAULT 0,
    moved_rows bigint NOT NULL DEFAULT 0,
    batch_size integer NOT NULL,
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    completed_at timestamptz,
    PRIMARY KEY (table_name, day_start),
    CHECK (source_rows >= 0),
    CHECK (staged_rows >= 0),
    CHECK (moved_rows >= 0),
    CHECK (batch_size BETWEEN 1 AND 100000)
);

COMMENT ON TABLE public.mtc_history_partition_backfill_state IS
    'Restartable operator state for moving rows out of request default partitions; not an application schema version.';

CREATE OR REPLACE PROCEDURE public.mtc_backfill_history_partition(
    p_table_name text,
    p_day_start date,
    p_batch_size integer DEFAULT 10000
)
LANGUAGE plpgsql
AS $procedure$
DECLARE
    v_time_column text;
    v_identity_column text;
    v_default_table text;
    v_stage_table text;
    v_target_table text;
    v_identity_index text;
    v_default_exclusion text;
    v_day_suffix text;
    v_range_start bigint;
    v_range_end bigint;
    v_batch_rows bigint;
    v_source_rows bigint;
    v_stage_rows bigint;
    v_moved_rows bigint;
    v_mismatch boolean;
    v_attached boolean;
BEGIN
    IF p_table_name = 'request_records' THEN
        v_time_column := 'created_at';
        v_identity_column := 'id';
    ELSIF p_table_name = 'request_events' THEN
        v_time_column := 'event_at';
        v_identity_column := 'event_id';
    ELSE
        RAISE EXCEPTION 'unsupported history table: %', p_table_name;
    END IF;

    IF p_batch_size < 1 OR p_batch_size > 100000 THEN
        RAISE EXCEPTION 'batch size must be between 1 and 100000';
    END IF;
    IF p_day_start >= (clock_timestamp() AT TIME ZONE 'UTC')::date THEN
        RAISE EXCEPTION 'only completed UTC days may be backfilled: %', p_day_start;
    END IF;

    v_day_suffix := to_char(p_day_start, 'YYYYMMDD');
    v_default_table := p_table_name || '_default';
    v_stage_table := p_table_name || '_mtc_stage_' || v_day_suffix;
    v_target_table := p_table_name || '_' || v_day_suffix;
    v_identity_index := p_table_name || '_bf_' || v_day_suffix || '_identity_uq';
    v_default_exclusion := 'mtc_bf_exclude_' || v_day_suffix;
    v_range_start := (
        extract(epoch FROM (p_day_start::timestamp AT TIME ZONE 'UTC')) * 1000
    )::bigint;
    v_range_end := (
        extract(epoch FROM ((p_day_start + 1)::timestamp AT TIME ZONE 'UTC')) * 1000
    )::bigint;

    IF to_regclass(format('public.%I', p_table_name)) IS NULL THEN
        RAISE EXCEPTION 'partitioned parent public.% does not exist', p_table_name;
    END IF;
    IF to_regclass(format('public.%I', v_default_table)) IS NULL THEN
        RAISE EXCEPTION 'default partition public.% does not exist', v_default_table;
    END IF;

    SELECT EXISTS (
        SELECT 1
          FROM pg_inherits inheritance
          JOIN pg_class parent_table ON parent_table.oid = inheritance.inhparent
          JOIN pg_namespace parent_namespace ON parent_namespace.oid = parent_table.relnamespace
          JOIN pg_class child_table ON child_table.oid = inheritance.inhrelid
          JOIN pg_namespace child_namespace ON child_namespace.oid = child_table.relnamespace
         WHERE parent_namespace.nspname = 'public'
           AND child_namespace.nspname = 'public'
           AND parent_table.relname = p_table_name
           AND child_table.relname = v_target_table
    ) INTO v_attached;

    IF v_attached THEN
        EXECUTE format(
            'SELECT count(*) FROM public.%I WHERE %I >= $1 AND %I < $2',
            v_default_table, v_time_column, v_time_column
        ) INTO v_source_rows USING v_range_start, v_range_end;
        IF v_source_rows <> 0 THEN
            RAISE EXCEPTION 'attached partition % still has % matching rows in %',
                v_target_table, v_source_rows, v_default_table;
        END IF;
        EXECUTE format('SELECT count(*) FROM public.%I', v_target_table)
            INTO v_stage_rows;
        INSERT INTO public.mtc_history_partition_backfill_state
            (table_name, day_start, status, source_rows, staged_rows, moved_rows,
             batch_size, updated_at, completed_at)
        VALUES
            (p_table_name, p_day_start, 'complete', v_stage_rows, v_stage_rows,
             v_stage_rows, p_batch_size, clock_timestamp(), clock_timestamp())
        ON CONFLICT (table_name, day_start) DO UPDATE SET
            status = 'complete',
            source_rows = excluded.source_rows,
            staged_rows = excluded.staged_rows,
            moved_rows = excluded.moved_rows,
            batch_size = excluded.batch_size,
            updated_at = excluded.updated_at,
            completed_at = excluded.completed_at;
        COMMIT;
        RAISE NOTICE '% % is already attached with % rows',
            p_table_name, p_day_start, v_stage_rows;
        RETURN;
    END IF;

    IF to_regclass(format('public.%I', v_target_table)) IS NOT NULL THEN
        RAISE EXCEPTION 'target relation public.% exists but is not attached', v_target_table;
    END IF;

    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS public.%I (LIKE public.%I INCLUDING ALL)',
        v_stage_table, p_table_name
    );

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = to_regclass(format('public.%I', v_stage_table))
           AND conname = 'mtc_backfill_bounds'
    ) THEN
        EXECUTE format(
            'ALTER TABLE public.%I ADD CONSTRAINT mtc_backfill_bounds CHECK (%I >= %s AND %I < %s)',
            v_stage_table, v_time_column, v_range_start, v_time_column, v_range_end
        );
    END IF;

    EXECUTE format(
        'CREATE UNIQUE INDEX IF NOT EXISTS %I ON public.%I (%I)',
        v_identity_index, v_stage_table, v_identity_column
    );

    INSERT INTO public.mtc_history_partition_backfill_state
        (table_name, day_start, status, batch_size, updated_at)
    VALUES
        (p_table_name, p_day_start, 'copying', p_batch_size, clock_timestamp())
    ON CONFLICT (table_name, day_start) DO UPDATE SET
        status = CASE
            WHEN mtc_history_partition_backfill_state.status = 'complete'
                THEN mtc_history_partition_backfill_state.status
            ELSE 'copying'
        END,
        batch_size = excluded.batch_size,
        updated_at = excluded.updated_at;
    COMMIT;

    LOOP
        EXECUTE format(
            'INSERT INTO public.%1$I '
            'SELECT source_row.* FROM public.%2$I source_row '
            'WHERE source_row.%3$I >= $1 AND source_row.%3$I < $2 '
            'AND NOT EXISTS ('
            '  SELECT 1 FROM public.%1$I staged_row '
            '  WHERE staged_row.%4$I = source_row.%4$I'
            ') '
            'ORDER BY source_row.%3$I ASC, source_row.%4$I ASC '
            'LIMIT $3 '
            'ON CONFLICT (%4$I) DO NOTHING',
            v_stage_table, v_default_table, v_time_column, v_identity_column
        ) USING v_range_start, v_range_end, p_batch_size;
        GET DIAGNOSTICS v_batch_rows = ROW_COUNT;

        EXECUTE format(
            'SELECT count(*) FROM public.%I WHERE %I >= $1 AND %I < $2',
            v_default_table, v_time_column, v_time_column
        ) INTO v_source_rows USING v_range_start, v_range_end;
        EXECUTE format('SELECT count(*) FROM public.%I', v_stage_table)
            INTO v_stage_rows;

        IF v_stage_rows > v_source_rows THEN
            RAISE EXCEPTION
                'staged row count % exceeds source row count % for % %',
                v_stage_rows, v_source_rows, p_table_name, p_day_start;
        END IF;

        UPDATE public.mtc_history_partition_backfill_state
           SET source_rows = v_source_rows,
               staged_rows = v_stage_rows,
               updated_at = clock_timestamp()
         WHERE table_name = p_table_name AND day_start = p_day_start;
        RAISE NOTICE '% % batch copied %, staged % of current source %',
            p_table_name, p_day_start, v_batch_rows, v_stage_rows, v_source_rows;
        COMMIT;

        EXIT WHEN v_batch_rows = 0;
    END LOOP;

    -- The final transaction is deliberately atomic.  Readers continue to see
    -- source rows until COMMIT, then see the attached daily partition.  The
    -- default-partition lock blocks only writes routed to this default table;
    -- reads and writes to already attached current partitions continue.
    EXECUTE format('LOCK TABLE public.%I IN SHARE ROW EXCLUSIVE MODE', v_default_table);

    -- Capture late historical imports that arrived during the copy phase.
    EXECUTE format(
        'INSERT INTO public.%1$I '
        'SELECT source_row.* FROM public.%2$I source_row '
        'WHERE source_row.%3$I >= $1 AND source_row.%3$I < $2 '
        'AND NOT EXISTS ('
        '  SELECT 1 FROM public.%1$I staged_row '
        '  WHERE staged_row.%4$I = source_row.%4$I'
        ') '
        'ON CONFLICT (%4$I) DO NOTHING',
        v_stage_table, v_default_table, v_time_column, v_identity_column
    ) USING v_range_start, v_range_end;

    EXECUTE format(
        'SELECT count(*) FROM public.%I WHERE %I >= $1 AND %I < $2',
        v_default_table, v_time_column, v_time_column
    ) INTO v_source_rows USING v_range_start, v_range_end;
    EXECUTE format('SELECT count(*) FROM public.%I', v_stage_table)
        INTO v_stage_rows;

    IF v_source_rows <> v_stage_rows THEN
        RAISE EXCEPTION
            'final count mismatch for % %: source %, staged %',
            p_table_name, p_day_start, v_source_rows, v_stage_rows;
    END IF;

    -- Count equality is followed by a bidirectional row comparison so a
    -- partially copied or changed row can never be attached silently.
    EXECUTE format(
        'SELECT EXISTS ('
        '  SELECT 1 FROM ('
        '    (SELECT * FROM public.%1$I WHERE %3$I >= $1 AND %3$I < $2 '
        '     EXCEPT SELECT * FROM public.%2$I) '
        '    UNION ALL '
        '    (SELECT * FROM public.%2$I '
        '     EXCEPT SELECT * FROM public.%1$I WHERE %3$I >= $1 AND %3$I < $2)'
        '  ) difference LIMIT 1'
        ')',
        v_default_table, v_stage_table, v_time_column
    ) INTO v_mismatch USING v_range_start, v_range_end;
    IF v_mismatch THEN
        RAISE EXCEPTION 'row-level validation failed for % %', p_table_name, p_day_start;
    END IF;

    UPDATE public.mtc_history_partition_backfill_state
       SET status = 'validating',
           source_rows = v_source_rows,
           staged_rows = v_stage_rows,
           updated_at = clock_timestamp()
     WHERE table_name = p_table_name AND day_start = p_day_start;

    EXECUTE format(
        'DELETE FROM public.%I WHERE %I >= $1 AND %I < $2',
        v_default_table, v_time_column, v_time_column
    ) USING v_range_start, v_range_end;
    GET DIAGNOSTICS v_moved_rows = ROW_COUNT;
    IF v_moved_rows <> v_source_rows THEN
        RAISE EXCEPTION
            'delete count mismatch for % %: expected %, deleted %',
            p_table_name, p_day_start, v_source_rows, v_moved_rows;
    END IF;

    -- A validated exclusion check lets ATTACH avoid rescanning the entire
    -- remaining default partition while holding its metadata lock.
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = to_regclass(format('public.%I', v_default_table))
           AND conname = v_default_exclusion
    ) THEN
        EXECUTE format(
            'ALTER TABLE public.%I ADD CONSTRAINT %I '
            'CHECK (%I < %s OR %I >= %s) NOT VALID',
            v_default_table, v_default_exclusion,
            v_time_column, v_range_start, v_time_column, v_range_end
        );
    END IF;
    EXECUTE format(
        'ALTER TABLE public.%I VALIDATE CONSTRAINT %I',
        v_default_table, v_default_exclusion
    );

    EXECUTE format(
        'ALTER TABLE public.%I RENAME TO %I',
        v_stage_table, v_target_table
    );
    EXECUTE format(
        'ALTER TABLE public.%I ATTACH PARTITION public.%I '
        'FOR VALUES FROM (%s) TO (%s)',
        p_table_name, v_target_table, v_range_start, v_range_end
    );
    EXECUTE format(
        'ALTER TABLE public.%I DROP CONSTRAINT %I',
        v_default_table, v_default_exclusion
    );

    UPDATE public.mtc_history_partition_backfill_state
       SET status = 'complete',
           source_rows = v_source_rows,
           staged_rows = v_stage_rows,
           moved_rows = v_moved_rows,
           updated_at = clock_timestamp(),
           completed_at = clock_timestamp()
     WHERE table_name = p_table_name AND day_start = p_day_start;
    COMMIT;

    RAISE NOTICE '% % attached with % validated rows',
        p_table_name, p_day_start, v_moved_rows;
END;
$procedure$;

COMMENT ON PROCEDURE public.mtc_backfill_history_partition(text, date, integer) IS
    'Copies one completed UTC day in committed batches, validates exact rows, then atomically replaces default-partition rows with an attached daily partition.';
