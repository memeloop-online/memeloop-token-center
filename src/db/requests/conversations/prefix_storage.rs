//! The observation fingerprint is a view of existing immutable prefix nodes.
//! It is deliberately distinct from the full semantic leaf: inference has
//! used at most 1024 atoms in the current writer; the leaf retains the entire sequence.
use std::collections::HashMap;

use super::*;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fingerprint {
    context_prefix_v1: Option<String>,
    atom_count: usize,
}

impl Fingerprint {
    pub(super) fn from_hashes(hashes: &[String]) -> Self {
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

struct StoredNode {
    parent: Option<String>,
    atom: String,
    depth: i64,
}

#[derive(Default)]
pub(super) struct PrefixPaths {
    nodes: HashMap<String, StoredNode>,
}

fn invalid_path() -> AppError {
    AppError::Conflict("conversation prefix path is missing or inconsistent".into())
}

impl PrefixPaths {
    pub(super) fn ancestors(&self, head: Option<&str>) -> Result<HashSet<String>, AppError> {
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
    pub(super) fn hashes(
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

fn decode(encoded: &str) -> Result<Fingerprint, AppError> {
    let fingerprint: Fingerprint = serde_json::from_str(encoded).map_err(|_| invalid_path())?;
    if fingerprint.atom_count > MAX_CONVERSATION_FINGERPRINT_ATOMS
        || (fingerprint.atom_count == 0) != fingerprint.context_prefix_v1.is_none()
    {
        return Err(invalid_path());
    }
    Ok(fingerprint)
}

pub(super) fn candidate_hashes(row: &AnyRow, paths: &PrefixPaths) -> Result<Vec<String>, AppError> {
    let encoded: Option<String> = row.try_get("atom_hashes_json")?;
    match encoded {
        Some(encoded) if encoded.starts_with('[') => {
            let mut hashes: Vec<String> =
                serde_json::from_str(&encoded).map_err(|_| invalid_path())?;
            hashes.truncate(MAX_CONVERSATION_FINGERPRINT_ATOMS);
            Ok(hashes)
        }
        Some(encoded) => {
            let fingerprint = decode(&encoded)?;
            paths.hashes(
                fingerprint.context_prefix_v1.as_deref(),
                Some(fingerprint.atom_count),
            )
        }
        None => {
            // Historical uncapped JSON is not sent over SQL merely to discard
            // its tail. Recover its bounded inference view from the full leaf.
            let leaf: Option<String> = row.try_get("leaf_node_hash")?;
            if leaf.is_none() {
                return Err(invalid_path());
            }
            let mut hashes = paths.hashes(leaf.as_deref(), None)?;
            hashes.truncate(MAX_CONVERSATION_FINGERPRINT_ATOMS);
            Ok(hashes)
        }
    }
}

pub(super) async fn load_candidate_paths(
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
        let encoded: Option<String> = row.try_get("atom_hashes_json")?;
        match encoded {
            Some(encoded) if encoded.starts_with('[') => {}
            Some(encoded) => {
                if let Some(head) = decode(&encoded)?.context_prefix_v1 {
                    roots.insert(head);
                }
            }
            None => {
                if let Some(leaf) = row.try_get::<Option<String>, _>("leaf_node_hash")? {
                    roots.insert(leaf);
                }
            }
        }
    }
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
mod tests;
