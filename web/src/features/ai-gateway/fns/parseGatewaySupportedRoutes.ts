import { z } from "zod/v4";

const gatewaySupportedRoutesSchema = z.array(
  z.enum(["openai.responses", "openai.chat-completions", "anthropic.messages"]),
);

/** The routes the Test-connection probe found on a custom origin. `null` when
 * the connection has never been probed. */
export function parseGatewaySupportedRoutes(value: unknown) {
  return gatewaySupportedRoutesSchema.nullable().parse(value);
}
