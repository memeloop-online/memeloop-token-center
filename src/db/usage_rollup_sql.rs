macro_rules! generation_dimensions_sql {
    ($table:literal, $bucket:literal, $divisor:literal, $modality:literal, $units:literal, $fact:literal, $guard:literal) => {
        concat!(
            "INSERT INTO ", $table, " (\n",
            "               tenant_id, key_id, ", $bucket, ", model, status_class, error_code,\n",
            r#"               upstream_account_id, model_route_id, modality, billing_unit,
               currency, units)
"#,
            "           SELECT tenant_id, key_id, created_at / ", $divisor, ", model, status_class,\n",
            "                  error_code, upstream_account_id, model_route_id, ", $modality, ",\n",
            "                  billing_unit, currency, ", $units, "\n",
            "             FROM ", $fact, "\n",
            "            WHERE ", $guard, "\n",
            "           ON CONFLICT (\n",
            "               tenant_id, key_id, ", $bucket, ", model, status_class, error_code,\n",
            "               upstream_account_id, model_route_id, modality, billing_unit, currency)\n",
            "           DO UPDATE SET units = ", $table, ".units + excluded.units",
        )
    };
}

pub(super) use generation_dimensions_sql;
