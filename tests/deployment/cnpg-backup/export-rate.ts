import assert from 'node:assert/strict';

export const exportRateSelection = `case "\${BACKUP_RATE_MIB_PER_SECOND:-1}" in
  1) backup_rate_chunk_bytes=65536 ;;
  4) backup_rate_chunk_bytes=262144 ;;
  8) backup_rate_chunk_bytes=524288 ;;
  *) printf 'Unsupported reviewed backup I/O rate\\n' >&2; exit 1 ;;
esac
export backup_rate_chunk_bytes`;

export const exportCompressionSelection = `case "\${BACKUP_COMPRESSION:-none}" in
  none) backup_compression=none ;;
  zstd:1) backup_compression=zstd:1 ;;
  *) printf 'Unsupported reviewed archive compression\\n' >&2; exit 1 ;;
esac
export backup_compression`;

export function withExportRate(script: string): string {
  const original = 'dd bs=65536 count=1 iflag=fullblock';
  assert.equal(script.split('umask 077').length, 2);
  assert.equal(script.split(original).length, 2);
  assert.equal(script.split('sleep 0.0625').length, 2);
  assert.equal(script.split('--compress=none').length, 2);
  return script
    .replace('umask 077', `umask 077\n${exportRateSelection}\n${exportCompressionSelection}`)
    .replace('--compress=none', '--compress="$backup_compression"')
    .replace(original, 'dd bs="$backup_rate_chunk_bytes" count=1 iflag=fullblock');
}
