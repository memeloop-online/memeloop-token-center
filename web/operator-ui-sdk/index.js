export const OPERATOR_UI_PACKAGE_API_V1 = 'operator-ui-package-v1';

export function defineOperatorUiPackage(value) {
  return value;
}

export function operatorUiPackageSupportsManifest(value, pluginId, pluginVersion) {
  return value.apiVersion === OPERATOR_UI_PACKAGE_API_V1
    && value.pluginId === pluginId
    && Array.isArray(value.compatiblePluginVersions)
    && value.compatiblePluginVersions.includes(pluginVersion);
}
