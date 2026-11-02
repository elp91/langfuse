import {
  Prisma,
  type GatewayConnectionStatus,
  type GatewayProvider,
  type PrismaClient,
} from "@langfuse/shared/src/db";
import {
  InvalidRequestError,
  LangfuseConflictError,
  LangfuseNotFoundError,
} from "@langfuse/shared";
import { decrypt, encrypt } from "@langfuse/shared/encryption";
import {
  LLMAdapter,
  testModelCall,
  validateLlmConnectionBaseURL,
} from "@langfuse/shared/src/server";
import { getDisplaySecretKey } from "@langfuse/shared/src/server/auth/apiKeys";

import { auditLog } from "@/src/features/audit-logs/server";
import { GATEWAY_CONNECTION_NAME_ERRORS } from "@/src/features/ai-gateway/constants/gatewayConnection";
import type { OrgAuthedContext } from "@/src/server/api/trpc";
import {
  type GatewayProviderName,
  getGatewayProviderDefinition,
} from "@/src/features/ai-gateway/server/provider/registry";
import { invalidateGatewayResolveCacheForOrganization } from "@/src/features/ai-gateway/server/resolve/gatewayResolveCache";
import { parseGatewayConnectionName } from "./parseGatewayConnectionName";
import {
  probeCustomRoutes,
  parseOrigin,
  type SupportedRoutes,
} from "./probeCustomRoutes";
import { GatewayProviderRepository } from "./gatewayProviderRepository";
import { GatewayModelCatalogService } from "../models/gatewayModelCatalogService";

export class GatewayProviderService {
  private readonly repository: GatewayProviderRepository;
  private readonly modelCatalogService: GatewayModelCatalogService;

  constructor(
    private readonly prisma: PrismaClient,
    private readonly validateCredential: CredentialValidator = validateGatewayCredential,
    modelCatalogService?: GatewayModelCatalogService,
  ) {
    this.repository = new GatewayProviderRepository(prisma);
    this.modelCatalogService =
      modelCatalogService ?? new GatewayModelCatalogService(prisma);
  }

  list(params: { organizationId: string; cursor?: string; limit: number }) {
    return this.repository.listConnections(params);
  }

  async get(params: { organizationId: string; id: string }) {
    const connection = await this.repository.getSafeConnection(params);
    if (!connection) throw new LangfuseNotFoundError("Provider credential");
    return connection;
  }

  async listAll(organizationId: string, status?: GatewayConnectionStatus) {
    const connections = [];
    let cursor: string | undefined;
    do {
      const page = await this.repository.listConnections({
        organizationId,
        cursor,
        limit: 100,
        status,
      });
      connections.push(...page.data);
      cursor = page.nextCursor ?? undefined;
    } while (cursor);
    return connections;
  }

  testCredential(params: { provider: GatewayProvider; credential: string }) {
    return this.validateCredential(params);
  }

  /** Test connection for CUSTOM providers: probe the origin's routes. */
  async testCustomConnection(params: { baseUrl?: string; credential: string }) {
    const baseUrl = await parseRequiredBaseUrl(params.baseUrl);
    return probeRoutes({
      baseUrl,
      credential: params.credential || null,
    });
  }

  async create(params: {
    organizationId: string;
    name: string;
    provider: GatewayProvider;
    credential: string;
    baseUrl?: string;
    session: OrgAuthedContext["session"];
  }) {
    const name = parseGatewayConnectionName(params.name);
    await this.assertNameAvailable({
      organizationId: params.organizationId,
      name,
    });
    // Only CUSTOM may omit a credential; every registry provider needs one
    // for the generation probe below.
    if (params.provider !== "CUSTOM" && !params.credential.trim()) {
      throw new InvalidRequestError("A provider credential is required");
    }
    await this.testCredential({
      provider: params.provider,
      credential: params.credential,
    });
    const baseUrl =
      params.provider === "CUSTOM"
        ? await parseRequiredBaseUrl(params.baseUrl)
        : null;
    // A keyless custom connection has nothing to authenticate with, so the
    // probe records no routes until a credential is added.
    const supportedRoutes =
      params.provider === "CUSTOM"
        ? await probeRoutesForCredential(params.credential, baseUrl)
        : null;
    let connection;
    try {
      connection = await this.repository.createConnection({
        organizationId: params.organizationId,
        name,
        provider: params.provider,
        baseUrl,
        // A keyless CUSTOM connection stores an empty column rather than an
        // empty ciphertext, which decrypt cannot parse.
        encryptedCredential: params.credential.trim()
          ? encrypt(params.credential)
          : "",
        displaySecret: getDisplaySecretKey(params.credential),
        createdById: params.session.user.id,
        status: "ENABLED",
        supportedRoutes,
      });
    } catch (error) {
      rethrowConnectionMutationError(error);
    }
    await invalidateGatewayResolveCacheForOrganization(params.organizationId);
    await auditLog(
      {
        session: params.session,
        resourceType: "gatewayAiConnection",
        resourceId: connection.id,
        action: "create",
        after: connection,
      },
      this.prisma,
    );
    return connection;
  }

  async update(params: {
    organizationId: string;
    id: string;
    name?: string;
    credential?: string;
    baseUrl?: string;
    status?: GatewayConnectionStatus;
    session: OrgAuthedContext["session"];
  }) {
    const existing = await this.repository.getSafeConnection({
      organizationId: params.organizationId,
      id: params.id,
    });
    if (!existing) throw new LangfuseNotFoundError("Provider credential");
    /** Unchanged names skip validation so pre-existing names stay editable. */
    const name =
      params.name === undefined || params.name.trim() === existing.name
        ? undefined
        : parseGatewayConnectionName(params.name);
    if (name !== undefined) {
      await this.assertNameAvailable({
        organizationId: params.organizationId,
        name,
        excludeId: existing.id,
      });
    }
    if (
      existing.status === "ERROR" &&
      params.status === "ENABLED" &&
      !params.credential
    ) {
      throw new InvalidRequestError(
        "Errored provider credentials need a credential update or a successful retry",
      );
    }
    if (params.credential) {
      await this.testCredential({
        provider: existing.provider,
        credential: params.credential,
      });
    }
    // Only a CUSTOM connection may drop its credential; the other providers
    // cannot authenticate without one.
    if (
      params.credential !== undefined &&
      !params.credential.trim() &&
      existing.provider !== "CUSTOM"
    ) {
      throw new InvalidRequestError("A provider credential is required");
    }

    const baseUrl =
      existing.provider === "CUSTOM"
        ? await parseRequiredBaseUrl(
            params.baseUrl ?? existing.baseUrl ?? undefined,
          )
        : null;
    const baseUrlChanged = baseUrl !== existing.baseUrl;
    // Re-probe when the credential or origin changes. A credential-only edit
    // probes with the new key; a base-URL-only edit probes with the stored
    // key, so an admin can move an authenticated connection without
    // re-entering its secret.
    let storedCredential: string | null = null;
    if (
      existing.provider === "CUSTOM" &&
      !params.credential &&
      baseUrlChanged
    ) {
      const row = await this.repository.getConnectionWithCredential({
        organizationId: params.organizationId,
        id: params.id,
      });
      storedCredential =
        row && row.encryptedCredential
          ? decrypt(row.encryptedCredential)
          : null;
    }
    const supportedRoutes =
      existing.provider === "CUSTOM" &&
      (params.credential !== undefined || baseUrlChanged)
        ? await probeRoutesForCredential(
            params.credential ?? storedCredential,
            baseUrl,
          )
        : undefined;

    let updated;
    try {
      // A keyless CUSTOM connection stores an empty column rather than an
      // empty ciphertext, which decrypt cannot parse.
      const encryptedCredential = credentialColumn(params.credential);
      updated = await this.repository.updateConnection({
        organizationId: params.organizationId,
        id: params.id,
        name,
        baseUrl,
        status: params.credential ? "ENABLED" : params.status,
        encryptedCredential,
        displaySecret: params.credential
          ? getDisplaySecretKey(params.credential)
          : undefined,
        supportedRoutes,
      });
    } catch (error) {
      rethrowConnectionMutationError(error);
    }
    await this.modelCatalogService.clearModelCache(
      params.organizationId,
      params.id,
    );
    await invalidateGatewayResolveCacheForOrganization(params.organizationId);
    await auditLog(
      {
        session: params.session,
        resourceType: "gatewayAiConnection",
        resourceId: params.id,
        action: "update",
        before: existing,
        after: updated,
      },
      this.prisma,
    );
    return updated;
  }

  async delete(params: {
    organizationId: string;
    id: string;
    session: OrgAuthedContext["session"];
  }) {
    // Only the two identifiers, never the whole params object: the extra
    // `session` field would land in the Prisma `where` and fail validation.
    const deleted = await this.repository.deleteConnection({
      organizationId: params.organizationId,
      id: params.id,
    });
    await this.modelCatalogService.clearModelCache(
      params.organizationId,
      params.id,
    );
    await invalidateGatewayResolveCacheForOrganization(params.organizationId);
    await auditLog(
      {
        session: params.session,
        resourceType: "gatewayAiConnection",
        resourceId: params.id,
        action: "delete",
        before: deleted,
      },
      this.prisma,
    );
    return deleted;
  }

  /** Names differing only by case read as the same slug prefix, so they count as taken. */
  private async assertNameAvailable(params: {
    organizationId: string;
    name: string;
    excludeId?: string;
  }) {
    if (await this.repository.hasConnectionNamedIgnoringCase(params)) {
      throw new InvalidRequestError(GATEWAY_CONNECTION_NAME_ERRORS.duplicate);
    }
  }

  async updateAllowedModels(params: {
    organizationId: string;
    id: string;
    allowedModels: string[] | null;
    expectedUpdatedAt: Date;
    session: OrgAuthedContext["session"];
  }) {
    const existing = await this.get({
      organizationId: params.organizationId,
      id: params.id,
    });
    const updated = await this.repository.updateAllowedModels({
      organizationId: params.organizationId,
      id: params.id,
      expectedUpdatedAt: params.expectedUpdatedAt,
      allowedModels:
        params.allowedModels === null
          ? null
          : [...new Set(params.allowedModels)],
    });
    if (!updated) {
      throw new LangfuseConflictError(
        "Model access changed while this page was open. Reload and try again.",
      );
    }
    await invalidateGatewayResolveCacheForOrganization(params.organizationId);
    await auditLog(
      {
        session: params.session,
        resourceType: "gatewayAiConnection",
        resourceId: params.id,
        action: "update",
        before: existing,
        after: updated,
      },
      this.prisma,
    );
    return updated;
  }
}

function rethrowConnectionMutationError(error: unknown): never {
  if (
    error instanceof Prisma.PrismaClientKnownRequestError &&
    error.code === "P2002"
  ) {
    throw new InvalidRequestError(GATEWAY_CONNECTION_NAME_ERRORS.duplicate);
  }
  throw error;
}

async function parseRequiredBaseUrl(
  baseUrl: string | undefined,
): Promise<string> {
  const origin = baseUrl === undefined ? null : parseOrigin(baseUrl);
  if (!origin) {
    throw new InvalidRequestError(
      "A custom provider connection requires a valid base URL (http or https, no query or credentials)",
    );
  }
  // Blocks private and reserved addresses (loopback, RFC1918, link-local),
  // honors the LLM connection exemption env vars, and enforces HTTPS on
  // Langfuse Cloud. The gateway re-checks egress at connect time; this check
  // keeps the Web-side probe and catalog fetch from issuing the request.
  try {
    await validateLlmConnectionBaseURL(origin);
  } catch (error) {
    throw new InvalidRequestError(
      error instanceof Error ? error.message : "Invalid base URL",
    );
  }
  return origin;
}

async function probeRoutes(params: {
  baseUrl: string;
  credential: string | null;
}): Promise<SupportedRoutes> {
  const result = await probeCustomRoutes({
    baseUrl: params.baseUrl,
    credential: params.credential,
    // Read at call time so tests can spy on globalThis.fetch.
    fetcher: (...args) => fetch(...args),
  });
  if (!result.success) {
    throw new InvalidRequestError(
      `Connection test failed: ${result.error.replace(/_/g, " ")}`,
    );
  }
  return result.routes;
}

/**
 * Credential column for a CUSTOM connection. A keyless connection stores an
 * empty column rather than an empty ciphertext, which decrypt cannot parse.
 */
function credentialColumn(credential: string | undefined): string | undefined {
  if (credential === undefined) {
    return undefined;
  }
  return credential.trim() ? encrypt(credential) : "";
}

/**
 * Routes for a CUSTOM connection given the credential in play. A keyless
 * connection has nothing to authenticate with, so it records no routes until
 * a credential is added.
 */
async function probeRoutesForCredential(
  credential: string | null | undefined,
  baseUrl: string | null,
): Promise<SupportedRoutes> {
  if (!credential?.trim()) {
    return [];
  }
  return probeRoutes({
    baseUrl: baseUrl as string,
    credential,
  });
}

async function validateGatewayCredential(params: {
  provider: GatewayProviderName;
  credential: string;
}): Promise<void> {
  const definition = getGatewayProviderDefinition(params.provider);
  // CUSTOM has no fixed origin or representative model; its Test connection is
  // the route probe, which runs against the connection's own baseUrl.
  if (params.provider === "CUSTOM") return;
  const adapter =
    params.provider === "ANTHROPIC" ? LLMAdapter.Anthropic : LLMAdapter.OpenAI;
  // After the CUSTOM return, only fixed-origin providers remain.
  const model = definition.validationModel as string;

  try {
    await testModelCall({
      provider: params.provider.toLowerCase(),
      model,
      apiKey: {
        id: "gateway-credential-validation",
        projectId: "gateway-credential-validation",
        createdAt: new Date(0),
        updatedAt: new Date(0),
        adapter,
        provider: params.provider.toLowerCase(),
        displaySecretKey: "",
        secretKey: encrypt(params.credential),
        extraHeaders: null,
        extraHeaderKeys: [],
        baseURL: definition.baseUrl as string,
        customModels: [],
        withDefaultModels: true,
        config: null,
      },
      timeout: 10_000,
    });
  } catch {
    throw new InvalidRequestError("Provider credential validation failed");
  }
}

type CredentialValidator = (params: {
  provider: GatewayProviderName;
  credential: string;
}) => Promise<void>;
