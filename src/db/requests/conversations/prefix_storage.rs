//! The observation fingerprint is a view of existing immutable prefix nodes.
//! It is deliberately distinct from the full semantic leaf: inference has
//! used at most 1024 atoms in the current writer; the leaf retains the entire sequence.
use std::collections::HashMap;

use super::*;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fingerprint {
    #[serde(deserialize_with = "required_head")]
    pub(crate) context_prefix_v1: Option<String>,
    pub(crate) atom_count: usize,
}

fn required_head<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    <Option<String> as serde::Deserialize>::deserialize(deserializer)
}

impl Fingerprint {
    pub(crate) fn from_hashes(hashes: &[String]) -> Self {
        let mut parent = None;
        for hash in hashes {
            parent = Some(crate::conversation::digest_parts(parent.as_deref(), hash));
        }
        Self {
            context_prefix_v1: parent,
            atom_count: hashes.len(),
        }
    }
}

pub(crate) struct StoredNode {
    pub(crate) parent: Option<String>,
    pub(crate) atom: String,
    pub(crate) depth: i64,
}

#[derive(Default)]
pub(crate) struct PrefixPaths {
    pub(crate) nodes: HashMap<String, StoredNode>,
}

fn invalid_path() -> AppError {
    AppError::Conflict("conversation prefix path is missing or inconsistent".into())
}

impl PrefixPaths {
    pub(crate) fn ancestors(&self, head: Option<&str>) -> Result<HashSet<String>, AppError> {
        let mut result = HashSet::new();
        let mut cursor = head;
        let mut child_depth = None;
        while let Some(hash) = cursor {
            let node = self.nodes.get(hash).ok_or_else(invalid_path)?;
            if child_depth.is_some_and(|depth| node.depth != depth - 1)
                || node.depth < 1
                || crate::conversation::digest_parts(node.parent.as_deref(), &node.atom) != hash
                || !result.insert(hash.to_owned())
            {
                return Err(invalid_path());
            }
            child_depth = Some(node.depth);
            cursor = node.parent.as_deref();
        }
        if child_depth.is_some_and(|depth| depth != 1) {
            return Err(invalid_path());
        }
        Ok(result)
    }

    /// With no expected count this reconstructs the complete leaf losslessly.
    /// The fingerprint reader supplies its exact count, never an empty fallback.
    pub(crate) fn hashes(
        &self,
        head: Option<&str>,
        count: Option<usize>,
    ) -> Result<Vec<String>, AppError> {
        let ancestors = self.ancestors(head)?;
        if count.is_some_and(|expected| expected != ancestors.len()) {
            return Err(invalid_path());
        }
        let mut hashes = Vec::with_capacity(ancestors.len());
        let mut cursor = head;
        while let Some(hash) = cursor {
            let node = self.nodes.get(hash).ok_or_else(invalid_path)?;
            hashes.push(node.atom.clone());
            cursor = node.parent.as_deref();
        }
        hashes.reverse();
        Ok(hashes)
    }
}

pub(crate) fn decode(encoded: &str) -> Result<Fingerprint, AppError> {
    let fingerprint: Fingerprint = serde_json::from_str(encoded).map_err(|_| invalid_path())?;
    if fingerprint.atom_count > MAX_CONVERSATION_FINGERPRINT_ATOMS
        || (fingerprint.atom_count == 0) != fingerprint.context_prefix_v1.is_none()
    {
        return Err(invalid_path());
    }
    Ok(fingerprint)
}

pub(crate) fn candidate_hashes(row: &AnyRow, paths: &PrefixPaths) -> Result<Vec<String>, AppError> {
    let encoded: String = row.try_get("atom_hashes_json")?;
    let fingerprint = decode(&encoded)?;
    paths.hashes(
        fingerprint.context_prefix_v1.as_deref(),
        Some(fingerprint.atom_count),
    )
}

pub(crate) async fn load_candidate_paths(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    candidates: &[AnyRow],
    current_leaf: Option<&str>,
) -> Result<PrefixPaths, ConversationContentError> {
    let mut roots = std::collections::BTreeSet::new();
    if let Some(leaf) = current_leaf {
        roots.insert(leaf.to_owned());
    }
    for row in candidates {
        let encoded: String = row.try_get("atom_hashes_json")?;
        if let Some(head) = decode(&encoded)?.context_prefix_v1 {
            roots.insert(head);
        }
    }
    load_paths(transaction, tenant_id, roots).await
}

pub(crate) async fn load_paths(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    roots: std::collections::BTreeSet<String>,
) -> Result<PrefixPaths, ConversationContentError> {
    if roots.is_empty() {
        return Ok(PrefixPaths::default());
    }
    // At most 50 candidates plus the current path. UNION collapses common
    // ancestors before recursion, so a shared 1024-atom prefix is visited once,
    // rather than once per observation. Both seed and recursive PK lookups are
    // tenant scoped. No context_nodes scan and no per-candidate SQL.
    let placeholders = (2..roots.len() + 2)
        .map(|index| format!("${index}"))
        .collect::<Vec<_>>()
        .join(",");
    let statement = format!(
        "WITH RECURSIVE paths(node_hash, parent_hash, atom_hash, depth) AS (SELECT node_hash, parent_hash, atom_hash, depth FROM context_nodes WHERE tenant_id = $1 AND node_hash IN ({placeholders}) UNION SELECT parent.node_hash, parent.parent_hash, parent.atom_hash, parent.depth FROM context_nodes parent JOIN paths child ON parent.node_hash = child.parent_hash AND parent.depth = child.depth - 1 WHERE parent.tenant_id = $1) SELECT node_hash, parent_hash, atom_hash, depth FROM paths"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(tenant_id);
    for root in &roots {
        query = query.bind(root);
    }
    let rows = query.fetch_all(&mut **transaction).await?;
    let mut paths = PrefixPaths::default();
    for row in rows {
        paths.nodes.insert(
            row.try_get("node_hash")?,
            StoredNode {
                parent: row.try_get("parent_hash")?,
                atom: row.try_get("atom_hash")?,
                depth: row.try_get("depth")?,
            },
        );
    }
    for root in roots {
        paths.ancestors(Some(&root))?;
    }
    Ok(paths)
}

#[cfg(test)]
pub(crate) mod tests;
