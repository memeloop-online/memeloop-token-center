CREATE TABLE IF NOT EXISTS conversation_key_clusters (
    key_id TEXT NOT NULL,
    cluster_id TEXT NOT NULL,
    explicit_session_id TEXT,
    updated_at BIGINT NOT NULL,
    request_count BIGINT NOT NULL,
    candidate_edge_count BIGINT NOT NULL,
    PRIMARY KEY (key_id, cluster_id)
);

CREATE INDEX IF NOT EXISTS conversation_key_clusters_page_idx
    ON conversation_key_clusters (key_id, updated_at DESC, cluster_id DESC);

CREATE INDEX IF NOT EXISTS conversation_observations_key_cluster_page_idx
    ON conversation_observations (key_id, cluster_id, created_at DESC, id DESC);

INSERT INTO conversation_key_clusters (
    key_id,
    cluster_id,
    explicit_session_id,
    updated_at,
    request_count,
    candidate_edge_count
)
SELECT members.key_id,
       members.cluster_id,
       members.explicit_session_id,
       members.updated_at,
       members.request_count,
       COALESCE(candidate_edges.candidate_edge_count, 0)
FROM (
    SELECT observations.key_id,
           observations.cluster_id,
           MIN(observations.explicit_session_id) AS explicit_session_id,
           MAX(observations.created_at) AS updated_at,
           COUNT(*) AS request_count
    FROM conversation_observations observations
    GROUP BY observations.key_id, observations.cluster_id
) members
LEFT JOIN (
    SELECT targets.key_id,
           edges.cluster_id,
           COUNT(*) AS candidate_edge_count
    FROM conversation_edges edges
    JOIN conversation_observations targets ON targets.id = edges.to_observation_id
    JOIN conversation_observations sources
      ON sources.id = edges.from_observation_id
     AND sources.key_id = targets.key_id
    WHERE edges.relation_kind = 'candidate'
    GROUP BY targets.key_id, edges.cluster_id
) candidate_edges
  ON candidate_edges.key_id = members.key_id
 AND candidate_edges.cluster_id = members.cluster_id;
