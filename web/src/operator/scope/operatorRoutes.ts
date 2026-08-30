import { operatorRouteKeys, type OperatorRouteKey } from '../../app/routes.js';

export { operatorRouteKeys };
export type { OperatorRouteKey };

export function isOperatorRouteKey(value: string): value is OperatorRouteKey {
  return (operatorRouteKeys as readonly string[]).includes(value);
}
