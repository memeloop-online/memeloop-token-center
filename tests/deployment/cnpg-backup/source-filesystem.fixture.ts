import { sourceFilesystem as expected } from './source-filesystem.ts';

export const sourceInventory = (capacityGiB = 30) => ({
  cluster: { metadata: { name: expected.cluster, namespace: expected.namespace }, spec: { instances: 1, storage: { size: `${capacityGiB}Gi` } }, status: { currentPrimary: expected.pod } },
  pod: {
    metadata: { name: expected.pod, namespace: expected.namespace, uid: expected.podUID },
    spec: { nodeName: expected.node, containers: [{ name: expected.container, env: [{ name: 'PGDATA', value: expected.pgdata }], volumeMounts: [{ name: 'pgdata', mountPath: expected.mount }] }], volumes: [{ name: 'pgdata', persistentVolumeClaim: { claimName: expected.claim } }] },
    status: { phase: 'Running', containerStatuses: [{ name: expected.container, ready: true, restartCount: 0, containerID: `containerd://${'a'.repeat(64)}`, state: { running: { startedAt: '2026-10-05T07:02:25Z' } } }] },
  },
  claim: { metadata: { name: expected.claim, namespace: expected.namespace, uid: expected.claimUID }, spec: { volumeName: expected.persistent, volumeMode: 'Filesystem', resources: { requests: { storage: `${capacityGiB}Gi` } } }, status: { phase: 'Bound', capacity: { storage: `${capacityGiB}Gi` } } },
  persistent: { metadata: { name: expected.persistent, uid: expected.persistentUID }, status: { phase: 'Bound' }, spec: {
    volumeMode: 'Filesystem', capacity: { storage: `${capacityGiB}Gi` }, claimRef: { name: expected.claim, namespace: expected.namespace, uid: expected.claimUID },
    csi: { driver: 'driver.longhorn.io', volumeHandle: expected.persistent, fsType: 'xfs' },
    nodeAffinity: { required: { nodeSelectorTerms: [{ matchExpressions: [{ key: 'kubernetes.io/hostname', operator: 'In', values: [expected.node] }] }] } },
  } },
});

export const sourceStatOutput = (availableBlocks = 2621440, totalBlocks = 7847936) => {
  const mount = `${expected.mount} /dev/longhorn/${expected.persistent} xfs / 65:48`;
  return `mount_before=${mount}\nstatfs=4096 ${totalBlocks} ${availableBlocks} ${availableBlocks} 15728640 15721786 xfs 413000000000\nmount_after=${mount}\n`;
};
