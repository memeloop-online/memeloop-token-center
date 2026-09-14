import { ADDITIONAL_PROPERTY_FLAG, type IconButtonProps, type WrapIfAdditionalTemplateProps } from '@rjsf/utils';
import { Button, Input } from '../design-system';
import { useI18n } from '../i18n';

/** Keep RJSF's rename/remove callbacks: keys and unknown values remain editable. */
export function AdditionalPropertyTemplate(props: WrapIfAdditionalTemplateProps) {
  const { locale } = useI18n();
  const { id, label, schema, children, classNames, style, disabled, readonly, onKeyRenameBlur, onRemoveProperty } = props;
  if (!(ADDITIONAL_PROPERTY_FLAG in schema)) return <div className={classNames} style={style}>{children}</div>;
  const zh = locale.startsWith('zh');
  return <div className={`provider-additional-property ${classNames}`} style={style}>
    <div className="provider-property-key">
      <label htmlFor={`${id}-key`}>{zh ? '字段名称' : 'Field name'}</label>
      <Input key={label} id={`${id}-key`} defaultValue={label} disabled={disabled} readOnly={readonly}
        aria-label={zh ? `字段名称：${label}` : `Field name: ${label}`} onBlur={onKeyRenameBlur} />
    </div>
    <div className="provider-property-value">{children}</div>
    <Button type="button" appearance="subtle" disabled={disabled || readonly} onClick={onRemoveProperty}
      aria-label={zh ? `删除字段：${label}` : `Remove field: ${label}`}>{zh ? '删除' : 'Remove'}</Button>
  </div>;
}

export function AddPropertyButton({ onClick, disabled, id }: IconButtonProps) {
  const { locale } = useI18n();
  return <Button id={id} type="button" appearance="secondary" disabled={disabled} onClick={onClick}>{locale.startsWith('zh') ? '添加字段' : 'Add field'}</Button>;
}
