import type { Plugin } from 'vite';

export function operatorFixturePlugin(fixture: string): Plugin {
  return {
    name: 'operator-navigation-fixture',
    configureServer(server) {
      server.middlewares.use((request, _response, next) => {
        const url = new URL(request.url ?? '/', 'http://fixture.invalid');
        if (url.pathname === '/operator') request.url = `${fixture}${url.search}`;
        next();
      });
    },
  };
}
