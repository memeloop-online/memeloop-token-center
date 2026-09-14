import { Button, Menu, MenuItem, MenuList, MenuPopover, MenuTrigger } from '@fluentui/react-components';

export interface CredentialAction {
  id: string; label: string; disabled?: boolean; description?: string;
  onSelect: () => void | Promise<void>;
}

export function CredentialActionMenu({ label, actions, disabled = false }: { label: string; actions: CredentialAction[]; disabled?: boolean }) {
  return <Menu><MenuTrigger disableButtonEnhancement><Button disabled={disabled} appearance="subtle">{label}</Button></MenuTrigger>
    <MenuPopover><MenuList>{actions.map(action => <MenuItem key={action.id} disabled={action.disabled} secondaryContent={action.description} onClick={() => void action.onSelect()}>{action.label}</MenuItem>)}</MenuList></MenuPopover>
  </Menu>;
}
