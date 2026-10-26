import type { RJSFSchema } from '@rjsf/utils';
import { localizeSchema, type Locale } from '../i18n';

export function groupStrategyCopy(id: string, schema: RJSFSchema, locale: Locale) {
  const zh = locale.startsWith('zh');
  const names: Record<string, [string, string, string, string]> = {
    'mtc-preferred-account': ['优先指定账号', 'Preferred accounts', '按指定顺序优先选择健康账号，适合固定账号偏好；不按额度重置时间排序。', 'Prefer healthy accounts in your chosen order for a fixed account preference; reset times do not affect this order.'],
    'mtc-soonest-reset': ['额度较早重置优先', 'Soonest quota reset', '适合优先使用较早重置的额度。仅在额度信息有效且有余量的账号之间调整顺序；其他账号保持原位。', 'Prefer earlier quota resets among accounts with fresh, positive remaining quota. Other accounts keep their positions.'],
    weighted: ['加权分配', 'Weighted distribution', '按配置的权重分配请求，适合容量不同的成员。', 'Distribute requests by configured weights for members with different capacities.'],
    'round-robin': ['轮流分配', 'Round robin', '依次选择成员，适合均匀分配请求。', 'Select members in turn to distribute requests evenly.'],
  };
  const known = names[id];
  const localized = localizeSchema(schema, locale);
  return {
    name: known ? known[zh ? 0 : 1] : localized.title || (zh ? '自定义策略' : 'Custom strategy'),
    description: known ? known[zh ? 2 : 3] : localized.description || (zh ? '按下方配置选择组内成员。' : 'Select group members using the configuration below.'),
  };
}
