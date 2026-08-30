import assert from 'node:assert/strict';
import { chromium, type Browser } from 'playwright';
import { assertSecureLiveURL, readCredentialFile } from '../security.js';

interface LiveConfiguration {
  controlURL: URL;
  gatewayURL: URL;
  serviceCredential: string;
  clientCredential: string;
  expectedKeyId: string;
  providerSecretCanary: string;
}

class LiveRuntime {
  private browser?: Browser;
  private configuration?: LiveConfiguration;

  async start(): Promise<void> {
    assert.equal(this.browser, undefined, 'live browser runtime must start only once');
    const controlURL = requiredURL('MTC_LIVE_CONTROL_URL');
    const gatewayURL = requiredURL('MTC_LIVE_GATEWAY_URL');
    const service = await readCredentialFile(requiredEnvironment('MTC_LIVE_SERVICE_CREDENTIAL_FILE'));
    const client = await readCredentialFile(requiredEnvironment('MTC_LIVE_CLIENT_CREDENTIAL_FILE'));
    const providerSecretCanary = requiredEnvironment('MTC_LIVE_PROVIDER_SECRET_CANARY');
    assert.match(
      providerSecretCanary, /^[A-Za-z0-9_-]{16,128}$/,
      'MTC_LIVE_PROVIDER_SECRET_CANARY must be a 16-128 character URL-safe sentinel',
    );
    assert.notEqual(providerSecretCanary, service.credential, 'provider secret canary must not equal a live credential');
    assert.notEqual(providerSecretCanary, client.credential, 'provider secret canary must not equal a live credential');
    assert.notEqual(
      controlURL.origin, gatewayURL.origin, 'control and gateway must use distinct HTTPS origins',
    );
    const environmentKeyId = process.env.MTC_LIVE_EXPECTED_KEY_ID?.trim();
    if (environmentKeyId && client.expectedKeyId) {
      assert.equal(environmentKeyId, client.expectedKeyId, 'configured stable key IDs do not match');
    }
    const expectedKeyId = environmentKeyId || client.expectedKeyId;
    assert.ok(expectedKeyId, 'MTC_LIVE_EXPECTED_KEY_ID or key_id in the client credential file is required');
    this.configuration = {
      controlURL,
      gatewayURL,
      serviceCredential: service.credential,
      clientCredential: client.credential,
      expectedKeyId,
      providerSecretCanary,
    };
    this.browser = await chromium.launch({ headless: true, executablePath: chromium.executablePath() });
  }

  async stop(): Promise<void> {
    const browser = this.browser;
    this.browser = undefined;
    this.configuration = undefined;
    await browser?.close();
  }

  requireBrowser(): Browser {
    assert.ok(this.browser, 'live browser runtime is not initialized');
    return this.browser;
  }

  requireConfiguration(): LiveConfiguration {
    assert.ok(this.configuration, 'live configuration is not initialized');
    return this.configuration;
  }
}

function requiredEnvironment(name: string): string {
  const value = process.env[name]?.trim();
  assert.ok(value, `${name} is required for live read-only acceptance`);
  return value;
}

function requiredURL(name: string): URL {
  const url = new URL(requiredEnvironment(name));
  assertSecureLiveURL(name, url);
  return url;
}

export const liveRuntime = new LiveRuntime();
