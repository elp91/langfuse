import type { ModelDiscoveryError } from "../types";

/** Cap on the JSON body a model catalog or probe reads from an upstream.
 * Hostile or misbehaving origins must not be able to exhaust Web memory. */
const MAX_JSON_BYTES = 4 * 1024 * 1024;

export type JsonRequestResult =
  | { success: true; response: Response; value: unknown }
  | { success: false; error: "timeout" | "provider_error" };

export async function requestJson(params: {
  fetcher: typeof fetch;
  url: URL;
  headers: Record<string, string>;
}): Promise<JsonRequestResult> {
  let response: Response;
  try {
    response = await params.fetcher(params.url, {
      method: "GET",
      headers: params.headers,
      signal: AbortSignal.timeout(10_000),
      redirect: "error",
    });
  } catch {
    return { success: false, error: "timeout" };
  }

  const declaredLength = Number(response.headers.get("content-length") ?? 0);
  if (declaredLength > MAX_JSON_BYTES) {
    return { success: false, error: "provider_error" };
  }
  let value: unknown;
  try {
    value = await readBoundedJson(response, MAX_JSON_BYTES);
  } catch {
    return { success: false, error: "provider_error" };
  }
  return { success: true, response, value };
}

async function readBoundedJson(
  response: Response,
  maxBytes: number,
): Promise<unknown> {
  if (!response.body) {
    return response.json().catch(() => null);
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > maxBytes) {
      await reader.cancel().catch(() => {});
      throw new Error("response body exceeds the JSON size cap");
    }
    chunks.push(value);
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return JSON.parse(new TextDecoder().decode(body));
}

export function standardHttpError(
  response: Response,
): ModelDiscoveryError | null {
  if (response.status === 401 || response.status === 403) return "unauthorized";
  if (response.status === 429) return "rate_limited";
  return response.ok ? null : "provider_error";
}
