use super::*;

#[test]
fn operator_history_uses_stable_identity_after_credential_purge() {
    let query = build_request_list_query(
        RequestListScope::Global,
        &RequestListFilter {
            limit: 25,
            ..Default::default()
        },
    );
    assert!(query.statement.contains("LEFT JOIN key_records k"));
    assert!(query.statement.contains("LEFT JOIN principals p"));
    assert!(query.statement.contains("__retired_credential__"));
    assert!(query.statement.contains("__retired_principal__"));
    assert!(
        !query
            .statement
            .contains("SELECT identity_key.id FROM key_records")
    );
}

#[tokio::test]
async fn postgres_operator_page_bounds_history_before_display_joins() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    for scope in [
        RequestListScope::Global,
        RequestListScope::Tenant("default"),
    ] {
        let query = build_request_list_query(
            scope,
            &RequestListFilter {
                limit: 100,
                lookahead: true,
                typed_ast: Some(TypedFilterAst {
                    logical_operator: crate::filter_ast::TypedFilterLogicalOperator::And,
                    conditions: vec![],
                }),
                ..Default::default()
            },
        );
        let mut statement = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (FORMAT TEXT, COSTS OFF) {}",
            query.statement
        )));
        for value in query.binds {
            statement = match value {
                RequestListBind::I64(value) => statement.bind(value),
                RequestListBind::Text(value) => statement.bind(value),
            };
        }
        let rows = statement.fetch_all(&database.pool).await.unwrap();
        let lines: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        let mut ancestors: Vec<(usize, &str)> = Vec::new();
        let mut native_scans = 0;
        for line in &lines {
            let indent = line.len() - line.trim_start().len();
            let node = line.trim_start().trim_start_matches("->").trim_start();
            // EXPLAIN annotations are not plan nodes.
            if !line.contains("->") && indent != 0 {
                continue;
            }
            while ancestors.last().is_some_and(|(depth, _)| *depth >= indent) {
                ancestors.pop();
            }
            if node.contains("Scan") && node.contains(" on request_records") {
                native_scans += 1;
                let bounded_source = ancestors
                    .iter()
                    .position(|(_, ancestor)| ancestor.starts_with("Subquery Scan on r"));
                assert!(
                    ancestors
                        .iter()
                        .enumerate()
                        .any(|(position, (_, ancestor))| {
                            ancestor.starts_with("Limit")
                                && bounded_source.is_some_and(|source| position > source)
                        }),
                    "historical scan must be bounded before display joins:\n{}",
                    lines.join("\n")
                );
            }
            ancestors.push((indent, node));
        }
        assert!(
            native_scans > 0,
            "the actual generated query must scan history"
        );
    }
}
