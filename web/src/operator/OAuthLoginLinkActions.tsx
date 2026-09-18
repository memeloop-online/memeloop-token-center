import { CopyButton } from '../CopyButton';
import { useI18n } from '../i18n';

/**
 * A login URL may need to be opened in another browser that has the selected
 * account egress. Keep the two actions adjacent for every OAuth adapter.
 */
export function OAuthLoginLinkActions({ url }: { url?: string }) {
  const { t } = useI18n();
  if (!url) return null;
  return <div className="button-row oauth-login-link-actions">
    <CopyButton value={url} label={t('common.copyAuthorizationUrl')} />
    <a className="button secondary" href={url} target="_blank" rel="noopener noreferrer">{t('common.openAuthorization')}</a>
  </div>;
}
