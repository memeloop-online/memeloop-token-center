import { Component, type ErrorInfo, type ReactNode } from 'react';

export interface RouteErrorCopy {
  eyebrow: string;
  title: string;
  description: string;
  retry: string;
  refresh: string;
}

interface RouteErrorBoundaryProps {
  children: ReactNode;
  copy: RouteErrorCopy;
  resetKey: string;
}

interface RouteErrorBoundaryState {
  error: Error | null;
}

export class RouteErrorBoundary extends Component<RouteErrorBoundaryProps, RouteErrorBoundaryState> {
  state: RouteErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): RouteErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Token Center route failed to render', error, info.componentStack);
  }

  componentDidUpdate(previousProps: RouteErrorBoundaryProps) {
    if (previousProps.resetKey !== this.props.resetKey && this.state.error) {
      this.setState({ error: null });
    }
  }

  // React.lazy caches a rejected import Promise. Clearing only boundary state
  // would immediately throw the same rejection again, so recovery must reload
  // the module graph rather than presenting a non-functional retry control.
  private retry = () => window.location.reload();
  private refresh = () => window.location.reload();

  render() {
    if (!this.state.error) return this.props.children;
    const { copy } = this.props;
    return <section className="route-error" role="alert" aria-live="assertive">
      <span className="eyebrow">{copy.eyebrow}</span>
      <h1>{copy.title}</h1>
      <p>{copy.description}</p>
      <div className="route-error-actions">
        <button type="button" onClick={this.retry}>{copy.retry}</button>
        <button type="button" className="secondary" onClick={this.refresh}>{copy.refresh}</button>
      </div>
    </section>;
  }
}
