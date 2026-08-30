import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { api } from '../api';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type { GenerationJob, ModelCatalogItem, ModelCatalogResponse } from '../types';
import { selfErrorMessage } from './errors';
import { buildGenerationInput, generationNeedsDuration } from './generationRequest';

export function GeneratePage({ credential, onError }: { credential: string; onError: (message: string) => void }) {
  const { locale, t } = useI18n();
  const [models, setModels] = useState<ModelCatalogItem[]>([]);
  const [kind, setKind] = useState<'image' | 'video'>('image');
  const [model, setModel] = useState('');
  const [prompt, setPrompt] = useState('');
  const [duration, setDuration] = useState('5');
  const [parameters, setParameters] = useState<Record<string, unknown>>({});
  const [loading, setLoading] = useState(true);
  const [catalogAvailable, setCatalogAvailable] = useState(false);
  const [catalogError, setCatalogError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [message, setMessage] = useState('');
  const sequence = useRef(0);
  const catalogController = useRef<AbortController | undefined>(undefined);
  const submitSequence = useRef(0);
  const submitController = useRef<AbortController | undefined>(undefined);

  async function loadCatalog() {
    const current = ++sequence.current;
    catalogController.current?.abort();
    const controller = new AbortController();
    catalogController.current = controller;
    setModels([]);
    setModel('');
    setParameters({});
    setCatalogAvailable(false);
    setCatalogError('');
    setLoading(true);
    onError('');
    try {
      const response = await api<ModelCatalogResponse>('/v1/models', credential, { signal: controller.signal });
      if (current !== sequence.current || controller.signal.aborted) return;
      const hasUsableCapabilities = response.data.length > 0
        && response.data.every((item) => Array.isArray(item.modalities));
      setModels(hasUsableCapabilities ? response.data : []);
      setCatalogAvailable(hasUsableCapabilities);
    } catch (reason) {
      if (current === sequence.current && !controller.signal.aborted) setCatalogError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (current === sequence.current) setLoading(false);
    }
  }

  useEffect(() => {
    submitSequence.current += 1;
    submitController.current?.abort();
    setKind('image');
    setPrompt('');
    setDuration('5');
    setMessage('');
    setSubmitting(false);
    void loadCatalog();
    return () => {
      sequence.current += 1;
      submitSequence.current += 1;
      catalogController.current?.abort();
      submitController.current?.abort();
    };
  }, [credential]);

  const generationModels = useMemo(() => catalogAvailable
    ? models.filter((item) => item.modalities?.includes(kind))
    : [], [models, catalogAvailable, kind]);
  const selectedModel = generationModels.find((item) => item.id === model);
  const selectedSchema = selectedModel?.generation_schema as RJSFSchema | undefined;
  const visibleSchema = useMemo<RJSFSchema | undefined>(() => {
    if (!selectedSchema) return undefined;
    const schema = structuredClone(selectedSchema);
    if (schema.properties) delete schema.properties.prompt;
    if (Array.isArray(schema.required)) schema.required = schema.required.filter((name) => name !== 'prompt');
    return schema;
  }, [selectedSchema]);
  const parameterErrors = selectedSchema
    ? validator.validateFormData({ ...parameters, prompt: prompt.trim() }, selectedSchema).errors
    : [];

  async function submit(event: FormEvent) {
    event.preventDefault();
    const selectedModelId = model.trim();
    const trimmedPrompt = prompt.trim();
    if (!catalogAvailable || !selectedModel || selectedModel.id !== selectedModelId || !selectedModel.modalities?.includes(kind) || !trimmedPrompt) {
      onError(t('self.modelModalityMismatch'));
      return;
    }
    if (selectedSchema && validator.validateFormData({ ...parameters, prompt: trimmedPrompt }, selectedSchema).errors.length) {
      onError(t('self.generationParametersInvalid'));
      return;
    }
    setSubmitting(true);
    setMessage('');
    onError('');
    const current = ++submitSequence.current;
    submitController.current?.abort();
    const controller = new AbortController();
    submitController.current = controller;
    try {
      const path = kind === 'video' ? '/v1/videos/generations' : '/v1/images/generations';
      const input = buildGenerationInput(kind, selectedModel, trimmedPrompt, duration, parameters);
      await api<GenerationJob>(path, credential, {
        method: 'POST',
        signal: controller.signal,
        headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ model: selectedModelId, input }),
      });
      if (current !== submitSequence.current || controller.signal.aborted) return;
      setMessage(t('self.generationSubmitted'));
      setPrompt('');
      setParameters({});
    } catch (reason) {
      if (submitController.current === controller && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('self.generationCreateFailed')));
    } finally {
      if (submitController.current === controller && !controller.signal.aborted) setSubmitting(false);
    }
  }

  return <div className="self-page self-generate-page" data-self-page="generate">
    <article className="panel form-panel generation-create">
      <div className="panel-title"><h2>{t('self.createGeneration')}</h2></div>
      {message && <div className="notice success" role="status">{message}</div>}
      <label>{t('self.generationKind')}<select value={kind} disabled={!catalogAvailable} onChange={(event) => { setKind(event.target.value as 'image' | 'video'); setModel(''); setParameters({}); }}><option value="image" disabled={catalogAvailable && !models.some((item) => item.modalities?.includes('image'))}>{t('self.image')}</option><option value="video" disabled={catalogAvailable && !models.some((item) => item.modalities?.includes('video'))}>{t('self.video')}</option></select></label>
      {loading ? <div className="boot">{t('common.loading')}</div> : !catalogAvailable ? <div className="notice warning" role="status">{catalogError || t('common.requestFailed')} <button type="button" className="secondary" onClick={() => void loadCatalog()}>{locale === 'zh-CN' ? '重试' : 'Retry'}</button></div> : generationModels.length === 0 ? <div className="notice warning" role="status">{t('self.noModelsForModality', { modality: t(`self.${kind}`) })}</div> : <form onSubmit={submit}>
        <label>{t('self.generationModel')}<select value={model} onChange={(event) => { setModel(event.target.value); setParameters({}); }}><option value="">{t('common.select')}</option>{generationModels.map((allowedModel) => <option value={allowedModel.id} key={allowedModel.id}>{allowedModel.id}</option>)}</select></label>
        <label>{t('self.generationPrompt')}<textarea value={prompt} onChange={(event) => setPrompt(event.target.value)} /></label>
        {generationNeedsDuration(kind, selectedModel) && <label>{t('self.generationDuration')}<input type="number" min="1" max="60" step="1" value={duration} onChange={(event) => setDuration(event.target.value)} /></label>}
        {visibleSchema && <div className="generation-parameters"><h3>{t('self.workflowParameters')}</h3><RjsfForm key={`${kind}-${model}-${locale}`} schema={localizeSchema(visibleSchema, locale)} formData={parameters} validator={validator} templates={schemaFormTemplates} tagName="div" noHtml5Validate onChange={({ formData }) => setParameters((formData ?? {}) as Record<string, unknown>)}><></></RjsfForm></div>}
        <button type="submit" disabled={!catalogAvailable || loading || submitting || !selectedModel || !prompt.trim() || parameterErrors.length > 0}>{submitting ? t('common.loading') : t('self.submitGeneration')}</button>
      </form>}
    </article>
  </div>;
}
