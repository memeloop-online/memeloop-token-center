import { createRoot } from 'react-dom/client';
import { MtcFluentProvider, Input, Button } from '../../src/design-system';
import '../../src/styles.css';
import '../../src/operator/operator.css';
import '../../src/operator/operatorFormSurfaces.css';
import '../../src/operator/formJourney.css';
import '../../src/operator/pages/systemSettings.css';
import '../../src/operator/upstreamQuota.css';
import '../../src/operator/managementSurfaces.css';
// Load the legacy theme last to exercise its higher-specificity light panel rule.
import '../../src/theme.css';

const route = new URLSearchParams(location.search).get('view') ?? 'routes';
const wrapper = route === 'providers' ? 'provider-layout' : route === 'pricing' ? 'pricing-page' : route === 'settings' ? 'system-settings' : route === 'plugins' ? '' : 'management-layout';
createRoot(document.getElementById('root')!).render(<MtcFluentProvider>
  <main className="app-main-content" data-surface="operator" data-route={route} style={{ padding: 20 }}>
    <div className={wrapper}>
      <article className={`panel ${route === 'settings' ? 'settings-card settings-access-card' : ''}`} data-testid="outer-surface">
        <h2>连接与管理</h2>
        <div className="managed-resource"><b>测试账号</b><p>状态与配置信息</p>
          <details className="inline-editor form-panel" open><summary>配置</summary><label>连接名称<Input value="测试连接" readOnly /></label><Button appearance="primary">保存配置</Button></details>
        </div>
        <div className="managed-resource"><b>第二个测试账号</b></div>
      </article>
    </div>
    <article className="create-journey" data-open="true"><div className="form-panel">
      <fieldset className="operator-form-section"><legend>身份与归属</legend><label>名称<Input value="草稿名称" readOnly /></label></fieldset>
    </div></article>
    <div className="upstream-quota-window" data-testid="excluded-quota">额度组件保持现有框线</div>
  </main>
  <main className="app-main-content" data-surface="operator" data-route="overview" hidden><article className="panel" data-testid="excluded-overview">总览保持现有卡片</article></main>
</MtcFluentProvider>);
