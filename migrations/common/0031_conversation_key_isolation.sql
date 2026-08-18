-- Repair projections created before conversation inference and display metadata
-- were fully scoped to a stable key. Cross-key edges remain as inert historical
-- evidence, but they are neither counted nor exposed through a key projection.
UPDATE conversation_key_clusters
SET explicit_session_id = (
        SELECT MIN(observations.explicit_session_id)
        FROM conversation_observations observations
        WHERE observations.key_id = conversation_key_clusters.key_id
          AND observations.cluster_id = conversation_key_clusters.cluster_id
    ),
    candidate_edge_count = (
        SELECT COUNT(*)
        FROM conversation_edges edges
        JOIN conversation_observations targets
          ON targets.id = edges.to_observation_id
        JOIN conversation_observations sources
          ON sources.id = edges.from_observation_id
         AND sources.key_id = targets.key_id
        WHERE edges.cluster_id = conversation_key_clusters.cluster_id
          AND edges.relation_kind = 'candidate'
          AND targets.key_id = conversation_key_clusters.key_id
    );
