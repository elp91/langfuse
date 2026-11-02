import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";

import { TooltipProvider } from "@/src/components/ui/tooltip";
import { LayerProvider } from "@/src/context/LayerContext/LayerContext";

import { GatewayAliasesPage } from "./GatewayAliasesPage";

const aliases = [
  {
    id: "alias-1",
    name: "gpt-fast",
    status: "ENABLED" as const,
    updatedAt: new Date("2026-01-01T00:00:00.000Z"),
    targets: [
      {
        id: "t-1",
        connectionId: "conn-1",
        connectionName: "OpenAI",
        providerModel: "gpt-4.1",
        position: 0,
      },
    ],
  },
];

const connections = [
  {
    id: "conn-1",
    name: "OpenAI",
    provider: "OPENAI",
    allowedModels: [],
    supportedRoutes: [],
  },
  {
    id: "conn-2",
    name: "Anthropic",
    provider: "ANTHROPIC",
    allowedModels: [],
    supportedRoutes: [],
  },
];

const mocks = vi.hoisted(() => ({
  upsertMutateAsync: vi.fn(async () => ({ id: "alias-2" })),
  invalidate: vi.fn(async () => undefined),
  capture: vi.fn(),
}));

vi.mock("@/src/utils/api", () => ({
  api: {
    useUtils: () => ({
      aiGateway: { listAliases: { invalidate: mocks.invalidate } },
    }),
    aiGateway: {
      listAliases: {
        useQuery: () => ({ isPending: false, isError: false, data: aliases }),
      },
      listModelConnections: {
        useQuery: () => ({
          isPending: false,
          isError: false,
          data: connections,
        }),
      },
      getSetupStatus: {
        useQuery: () => ({ isPending: false, data: {} }),
      },
      upsertAlias: {
        useMutation: () => ({
          mutateAsync: mocks.upsertMutateAsync,
          isPending: false,
          error: null,
          reset: vi.fn(),
        }),
      },
      deleteAlias: {
        useMutation: () => ({
          mutateAsync: vi.fn(),
          isPending: false,
          error: null,
          reset: vi.fn(),
        }),
      },
    },
  },
}));

vi.mock("@/src/features/posthog-analytics/usePostHogClientCapture", () => ({
  usePostHogClientCapture: () => mocks.capture,
}));

vi.mock("@/src/features/rbac", () => ({
  useHasOrganizationAccess: () => true,
}));

vi.mock("next-auth/react", () => ({
  useSession: () => ({
    status: "authenticated",
    data: { environment: { aiGatewayDeployed: true } },
  }),
}));

vi.mock("@/src/features/feature-flags", () => ({
  useIsFeatureEnabled: () => true,
}));

async function openDialog() {
  render(
    <LayerProvider>
      <TooltipProvider>
        <GatewayAliasesPage organizationId="org-1" />
      </TooltipProvider>
    </LayerProvider>,
  );
  fireEvent.click(screen.getByRole("button", { name: "New alias" }));
  await screen.findByRole("dialog", undefined, { timeout: 3000 });
}

describe("GatewayAliasesPage alias dialog", () => {
  beforeAll(() => {
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    );
    Element.prototype.scrollIntoView = vi.fn();
  });

  it("saves a new alias with two targets in visual order", async () => {
    await openDialog();

    fireEvent.change(screen.getByLabelText("Name"), {
      target: { value: "claude-fallback" },
    });
    // First target row: connection then upstream model.
    fireEvent.click(
      screen.getByRole("combobox", { name: "Target 1 connection" }),
    );
    fireEvent.click(await screen.findByText("OpenAI"));
    fireEvent.change(screen.getByLabelText("Target 1 model"), {
      target: { value: "claude-sonnet-4" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add target" }));
    fireEvent.click(
      screen.getByRole("combobox", { name: "Target 2 connection" }),
    );
    fireEvent.click(await screen.findByText("Anthropic"));
    fireEvent.change(screen.getByLabelText("Target 2 model"), {
      target: { value: "claude-haiku-4" },
    });

    fireEvent.click(screen.getByRole("button", { name: "Save alias" }));

    await waitFor(() =>
      expect(mocks.upsertMutateAsync).toHaveBeenCalledWith({
        orgId: "org-1",
        name: "claude-fallback",
        status: "ENABLED",
        targets: [
          { connectionId: "conn-1", providerModel: "claude-sonnet-4" },
          { connectionId: "conn-2", providerModel: "claude-haiku-4" },
        ],
      }),
    );
    expect(mocks.invalidate).toHaveBeenCalledWith({ orgId: "org-1" });
  });
});
