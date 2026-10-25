import type { RJSFSchema } from '@rjsf/utils';
import { localizeSchema, type Locale } from '../i18n';

export function groupStrategyCopy(id: string, schema: RJSFSchema, locale: Locale) {
  const zh = locale.startsWith('zh');
  const names: Record<string, [string, string, string, string]> = {
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
