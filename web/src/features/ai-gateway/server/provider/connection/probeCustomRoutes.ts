import type { GatewayApiFormat } from "@/src/features/ai-gateway/server/provider/registry";
import { requestJson } from "@/src/features/ai-gateway/server/provider/models/adapters/shared";

export type SupportedRoutes = Array<
  "openai.responses" | "openai.chat-completions" | "anthropic.messages"
>;

export type RouteProbeResult =
  | { success: true; routes: SupportedRoutes }
  | { success: false; error: string };

/** Minimal per-route request bodies. Upstream validation errors (400/422) count
 * as "route exists": the point is reachability, not a successful generation. */
const ROUTE_PROBES: ReadonlyArray<{
  apiFormat: GatewayApiFormat;
  path: string;
  body: Record<string, unknown>;
}> = [
  {
    apiFormat: "openai.chat-completions",
    path: "/chat/completions",
    body: { model: "probe", messages: [], max_tokens: 1 },
  },
  {
    apiFormat: "openai.responses",
    path: "/responses",
    body: { model: "probe", input: "", max_output_tokens: 1 },
  },
  {
    apiFormat: "anthropic.messages",
    path: "/messages",
    body: { model: "probe", max_tokens: 1, messages: [] },
  },
];

/** Which OpenAI-compatible routes a custom origin actually serves. Probes are
 * reachability checks: 404/405 means absent, anything else (including 4xx
 * validation errors) means present. A key that works on /models is assumed to
 * work on the routes. */
export async function probeCustomRoutes(params: {
  baseUrl: string;
  credential: string | null;
  fetcher: typeof fetch;
}): Promise<RouteProbeResult> {
  const origin = parseOrigin(params.baseUrl);
  if (!origin) {
    return { success: false, error: "invalid_base_url" };
  }

  const headers: Record<string, string> = {};
  if (params.credential) {
    headers.Authorization = `Bearer ${params.credential}`;
  }

  const catalog = await requestJson({
    fetcher: params.fetcher,
    url: new URL(`${origin}/models`),
    headers,
  });
  if (!catalog.success) {
    return { success: false, error: "unreachable" };
  }
  if (!catalog.response.ok) {
    return {
      success: false,
      error:
        catalog.response.status === 401 || catalog.response.status === 403
          ? "unauthorized"
          : "unreachable",
    };
  }

  const routes: SupportedRoutes = [];
  await Promise.all(
    ROUTE_PROBES.map(async (probe) => {
      const reachable = await routeReachable({
        fetcher: params.fetcher,
        url: new URL(`${origin}${probe.path}`),
        headers: { ...headers, "Content-Type": "application/json" },
        body: JSON.stringify(probe.body),
      });
      if (reachable) routes.push(probe.apiFormat);
    }),
  );
  return { success: true, routes };
}

async function routeReachable(params: {
  fetcher: typeof fetch;
  url: URL;
  headers: Record<string, string>;
  body: string;
}): Promise<boolean> {
  let response: Response;
  try {
    response = await params.fetcher(params.url, {
      method: "POST",
      headers: params.headers,
      body: params.body,
      signal: AbortSignal.timeout(10_000),
      redirect: "error",
    });
  } catch {
    return false;
  }
  if (response.status !== 404 && response.status !== 405) return true;
  // A served route can still answer 404 from the application (vLLM: unknown
  // model). A router-level miss is the framework's plain-text not-found body
  // (FastAPI: {"detail":"Not Found"}), which no LLM API error uses.
  const body = await response.text().catch(() => "");
  try {
    const parsed: unknown = JSON.parse(body);
    return typeof parsed === "object" && parsed !== null && "error" in parsed;
  } catch {
    return false;
  }
}

/** The scheme+host+port+path prefix of a base URL, without a trailing slash. */
export function parseOrigin(baseUrl: string): string | null {
  try {
    const url = new URL(baseUrl);
    if (url.protocol !== "http:" && url.protocol !== "https:") return null;
    if (url.username || url.password || url.search || url.hash) return null;
    if (!url.hostname) return null;
    return url.origin + url.pathname.replace(/\/+$/, "");
  } catch {
    return null;
  }
}
