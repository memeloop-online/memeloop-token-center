import { CopyButton } from '../CopyButton.js';
import { useI18n } from '../i18n';
import { SecretInput } from '../SecretInput';
import './pages/systemSettings.css';

export interface OperatorAccessSettingsProps {
  credentialInput: string;
  credential: string;
  authenticating?: boolean;
  onCredentialInput: (value: string) => void;
  onConnect: (value: string) => void;
  onClear: () => void;
}

/** Small authentication surface kept outside the tenant settings bundle. */
export function OperatorAccessSettings({ credentialInput, credential, authenticating = false, onCredentialInput, onConnect, onClear }: OperatorAccessSettingsProps) {
  const { t } = useI18n();
  return <article className="panel settings-card settings-access-card">
    <div className="settings-card-heading">
      <div>
        <h3>{t('settings.accessTitle')}</h3>
        <p className="muted">{t('settings.accessDescription')}</p>
      </div>
      {credential && <span className="status ok">{t('common.savedCredentialInUse')}</span>}
    </div>
    <form className="system-settings-access operator-credential" aria-busy={authenticating} onSubmit={(event) => {
      event.preventDefault();
      const submittedCredential = new FormData(event.currentTarget).get('credential');
      if (typeof submittedCredential === 'string' && submittedCredential.trim()) onConnect(submittedCredential);
    }}>
      <div><label htmlFor="operator-access-credential">{t('settings.accessCredential')}</label><SecretInput id="operator-access-credential" name="credential" label={t('settings.accessCredential')} autoComplete="off" value={credentialInput} onChange={(event) => onCredentialInput(event.target.value)} placeholder={t('operator.tokenPlaceholder')} /></div>
      <div className="button-row"><button type="submit" disabled={!credentialInput.trim()}>{credential ? t('settings.replaceCredential') : t('common.connect')}</button>{credentialInput.trim() && <CopyButton value={credentialInput} label={t('common.copySecret')} />}{credential && <><CopyButton value={credential} label={t('common.copySecret')} /><button type="button" className="secondary" onClick={onClear}>{t('common.clearCredential')}</button></>}</div>
    </form>
  </article>;
}
