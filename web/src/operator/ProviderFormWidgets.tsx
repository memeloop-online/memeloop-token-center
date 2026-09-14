import type { WidgetProps } from '@rjsf/utils';
import { CopyButton } from '../CopyButton';
import { fluentFormWidgets } from './FluentFormWidgets';

/** Only values already returned to this authorized editor are copyable. */
function ProviderTextWidget(props: WidgetProps) {
  const TextWidget = fluentFormWidgets.TextWidget;
  return <div className="provider-config-value">
    <TextWidget {...props} />
    {typeof props.value === 'string' && props.value && !props.disabled && <CopyButton value={props.value} />}
  </div>;
}

export const providerFormWidgets = { ...fluentFormWidgets, TextWidget: ProviderTextWidget };
