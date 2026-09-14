import type { Plugin } from 'vite';

/** configFile:false fixtures serve public/ at /, unlike the production asset base. */
export function fixtureAssets(): Plugin {
  return {
    name: 'fixture-public-assets',
    configureServer(server) {
      server.middlewares.use((request, _response, next) => {
        if (request.url === '/ui-assets/token-center-icon-32.png') request.url = '/token-center-icon-32.png';
        next();
      });
    },
  };
}
