import { Button, Select } from '../design-system';
import { useI18n } from '../i18n';
import { usagePresets, type Granularity, type Preset } from './usageState';

export type QuickRangePreset = Preset | 'alltime';

/**
 * Quick time-range controls shared by the request list and usage analysis
 * filter dialog.  Requests may additionally drop the period entirely
 * (alltime); usage analysis always resolves to a bounded range.  Custom
 * ranges keep millisecond precision so a saved preset round-trips unchanged.
 */
export function FilterTimeRangeControls({ allowAllTime = false, disabled = false, granularity, onGranularityChange, onSelectPreset, preset }: {
  allowAllTime?: boolean;
  disabled?: boolean;
  granularity?: Granularity;
  onGranularityChange?: (granularity: Granularity) => void;
  onSelectPreset: (preset: QuickRangePreset) => void;
  preset: QuickRangePreset;
}) {
  const { t } = useI18n();
  const options: QuickRangePreset[] = allowAllTime ? [...usagePresets, 'alltime'] : [...usagePresets];
  return <section className="usage-controls typed-filter-period">
    <fieldset>
      <legend>{t('usage.timeRange')}</legend>
      <div className="usage-presets">
        {options.map((option) => <Button key={option} type="button" appearance="secondary" className={preset === option ? 'active' : undefined} aria-pressed={preset === option} disabled={disabled} onClick={() => onSelectPreset(option)}>{t(`usage.preset.${option}`)}</Button>)}
      </div>
    </fieldset>
    {granularity !== undefined && onGranularityChange && <label className="typed-filter-granularity"><span>{t('usage.granularity')}</span><Select value={granularity} disabled={disabled} onChange={(_, data) => onGranularityChange(data.value as Granularity)}><option value="auto">{t('usage.granularity.auto')}</option><option value="hour">{t('usage.granularity.hour')}</option><option value="day">{t('usage.granularity.day')}</option></Select></label>}
  </section>;
}
