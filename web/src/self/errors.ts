import { ApiError } from '../api';

export function selfErrorMessage(reason: unknown, t: (key: string) => string, fallback: string) {
  if (reason instanceof ApiError) {
    if (reason.code === 'unauthorized' || reason.status === 401) return t('self.invalidCredential');
    if (reason.code === 'invalid_request' || reason.status === 400) return t('self.invalidFilter');
    if (reason.code === 'forbidden' || reason.status === 403) return t('self.readPermissionDenied');
    if (reason.code === 'not_found' || reason.status === 404) return t('self.resourceMissing');
    if (reason.code === 'insufficient_quota') return t('self.insufficientQuota');
    if (reason.code === 'rate_limit_exceeded' || reason.status === 429) return t('self.rateLimited');
    if (reason.code === 'unpriced_model') return t('self.unpricedModel');
    if (reason.status >= 500) return t('self.temporarilyUnavailable');
  }
  return reason instanceof Error && !(reason instanceof TypeError) ? reason.message : fallback;
}
