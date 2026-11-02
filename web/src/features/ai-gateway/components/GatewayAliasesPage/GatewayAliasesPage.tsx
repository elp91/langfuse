import type { ComponentProps } from "react";
import { Plus, Trash2 } from "@langfuse/icons";

import Header from "@/src/components/layouts/header";
import { Alert } from "@/src/components/design-system/Alert/Alert";
import { ConfirmationDialogController } from "@/src/components/design-system/ConfirmationDialogController/ConfirmationDialogController";
import { createStatusTableColumn } from "@/src/components/design-system/table/columns/createStatusTableColumn";
import { createTextTableColumn } from "@/src/components/design-system/table/columns/createTextTableColumn";
import { createTableColumn } from "@/src/components/design-system/table/columns/utils/createTableColumn";
import type { AsyncTableData } from "@/src/components/design-system/table/Table";
import { SettingsTable } from "@/src/components/SettingsTable/SettingsTable";
import type { LangfuseColumnDef } from "@/src/components/table/types";
import { Button } from "@/src/components/ui/button";
import { Skeleton } from "@/src/components/ui/skeleton";
import { BadgeShell } from "@/src/components/design-system/Badge/Badge";
import { Tooltip } from "@/src/components/design-system/Tooltip/Tooltip";
import { AliasDialogController } from "@/src/features/ai-gateway/components/GatewayAliasesPage/components/AliasDialogController";
import { GatewayNotDeployedNotice } from "@/src/features/ai-gateway/components/GatewayNotDeployedNotice/GatewayNotDeployedNotice";
import { GatewaySetupEmptyState } from "@/src/features/ai-gateway/components/GatewaySetupEmptyState/GatewaySetupEmptyState";
import { providerIcons } from "@/src/features/ai-gateway/constants/providerIcons";
import { useGatewaySettingsAccess } from "@/src/features/ai-gateway/hooks/useGatewaySettingsAccess";
import { usePostHogClientCapture } from "@/src/features/posthog-analytics/usePostHogClientCapture";
import { api, type RouterOutputs } from "@/src/utils/api";

type GatewayAliasRow = RouterOutputs["aiGateway"]["listAliases"][number];

type AliasTarget = GatewayAliasRow["targets"][number];

/** Position of the first target whose connection will serve. */
function firstHealthyTargetIndex(targets: AliasTarget[]): number {
  return targets.findIndex(
    (candidate) => candidate.connectionStatus === "ENABLED",
  );
}

/** Only a target that can actually serve needs attention: the first healthy
 * one is green; everything after it is grey until the targets above it fail.
 * Errored and disabled connections are highlighted because they will not
 * serve. */
function targetBadgeColor(
  target: AliasTarget,
  index: number,
  firstHealthyIndex: number,
): NonNullable<ComponentProps<typeof BadgeShell>["color"]> {
  if (target.connectionStatus !== "ENABLED") {
    return target.connectionStatus === "DISABLED" ? "yellow" : "red";
  }
  return index === firstHealthyIndex ? "green" : "filled";
}

function targetStatusHint(
  target: AliasTarget,
  index: number,
  firstHealthyIndex: number,
): string {
  if (target.connectionStatus === "ENABLED") {
    return index === firstHealthyIndex
      ? "will serve requests"
      : "serves only when the targets above it fail";
  }
  return target.connectionStatus === "DISABLED"
    ? "is disabled; requests fall through to the next target"
    : "errored; requests fall through to the next target";
}

/** Targets stack in fallback order; the arrow points from each badge down to
 * the next one. */
function AliasTargetsCell({ targets }: { targets: AliasTarget[] }) {
  const firstHealthyIndex = firstHealthyTargetIndex(targets);
  return (
    <div className="flex flex-col items-start gap-1.5">
      {targets.map((target, index) => {
        const ProviderIcon = providerIcons[target.connectionProvider];
        return (
          <span key={target.id} className="flex flex-col items-start gap-0.5">
            <Tooltip
              label={`Target ${index + 1}: ${target.connectionName} ${targetStatusHint(target, index, firstHealthyIndex)}`}
            >
              {({ getTriggerProps }) => (
                <span
                  {...getTriggerProps()}
                  tabIndex={0}
                  className="focus-visible:ring-ring rounded-sm focus-visible:ring-2 focus-visible:outline-hidden"
                >
                  <BadgeShell
                    color={targetBadgeColor(target, index, firstHealthyIndex)}
                  >
                    <ProviderIcon aria-hidden size="sm" />
                    <span
                      className="truncate py-0.5"
                      title={target.connectionName}
                    >
                      {target.connectionName}
                    </span>
                    <span className="text-muted-foreground">·</span>
                    <span className="font-mono text-xs">
                      {target.providerModel}
                    </span>
                  </BadgeShell>
                </span>
              )}
            </Tooltip>
            {index < targets.length - 1 ? (
              <span
                aria-hidden
                className="text-muted-foreground ml-4 text-xs leading-none"
              >
                ↓
              </span>
            ) : null}
          </span>
        );
      })}
    </div>
  );
}

export function GatewayAliasesPage({
  organizationId,
}: {
  organizationId: string;
}) {
  const { canManageGateway, isIngestionConfigured, isSetupStatusPending } =
    useGatewaySettingsAccess(organizationId);

  if (isSetupStatusPending) {
    return <AliasesSkeleton />;
  }
  /** Managers are redirected to Setup by the settings route. */
  if (!canManageGateway && isIngestionConfigured === false) {
    return <GatewayNotDeployedNotice />;
  }
  return (
    <div className="flex flex-col gap-4">
      <div>
        <Header title="Model aliases" />
        <p className="text-muted-foreground text-sm">
          Model aliases map a stable model name to one or more provider models.
          Requests try the targets in order and fall back to the next one when a
          connection is unavailable.
        </p>
      </div>
      <ConnectedGatewayAliasesTable
        key={organizationId}
        organizationId={organizationId}
        canManage={canManageGateway}
      />
    </div>
  );
}

function ConnectedGatewayAliasesTable({
  organizationId,
  canManage,
}: {
  organizationId: string;
  canManage: boolean;
}) {
  const aliasesQuery = api.aiGateway.listAliases.useQuery({
    orgId: organizationId,
  });
  const connectionsQuery = api.aiGateway.listModelConnections.useQuery({
    orgId: organizationId,
  });
  const utils = api.useUtils();
  const capture = usePostHogClientCapture();
  const upsert = api.aiGateway.upsertAlias.useMutation();
  const remove = api.aiGateway.deleteAlias.useMutation();

  const invalidateAliases = () =>
    utils.aiGateway.listAliases.invalidate({ orgId: organizationId });

  if (aliasesQuery.isError || connectionsQuery.isError) {
    return (
      <div className="flex flex-col gap-4">
        <Alert variant="destructive">
          <Alert.Title>Model aliases could not be loaded</Alert.Title>
          <Alert.Description>
            Retry to manage aliases for this organization.
          </Alert.Description>
        </Alert>
        <Button
          className="w-fit"
          variant="secondary"
          onClick={() => {
            aliasesQuery.refetch();
            connectionsQuery.refetch();
          }}
        >
          Retry
        </Button>
      </div>
    );
  }

  const data: AsyncTableData<GatewayAliasRow[]> = aliasesQuery.isPending
    ? { status: "loading" }
    : { status: "success", data: aliasesQuery.data };
  const hasNoAliases = data.status === "success" && data.data.length === 0;

  const columns: LangfuseColumnDef<GatewayAliasRow>[] = [
    createTextTableColumn<GatewayAliasRow>({
      accessorFn: (row) => row.name,
      id: "name",
      header: "Alias",
      enableResizing: false,
    }),
    createStatusTableColumn<GatewayAliasRow, string>({
      id: "status",
      header: "Status",
      accessorFn: (row) => row.status,
      getStatus: (value) => (value === "ENABLED" ? "active" : "inactive"),
      enableResizing: false,
    }),
    createTableColumn<GatewayAliasRow, AliasTarget[]>({
      accessorKey: "targets",
      header: "Targets",
      loadingCell: <Skeleton className="h-5 w-24 shrink-0 rounded-sm" />,
      renderCell: (targets) =>
        targets ? <AliasTargetsCell targets={targets} /> : null,
      enableResizing: false,
    }),
  ];

  const toggleStatus = async (alias: GatewayAliasRow) => {
    const status = alias.status === "ENABLED" ? "DISABLED" : "ENABLED";
    try {
      await upsert.mutateAsync({
        orgId: organizationId,
        id: alias.id,
        name: alias.name,
        status,
        targets: alias.targets.map((target) => ({
          connectionId: target.connectionId,
          providerModel: target.providerModel,
        })),
      });
      capture("ai_gateway:alias_status_changed", {
        orgId: organizationId,
        status,
      });
      await invalidateAliases();
    } catch {
      /** tRPC errors surface as toasts; the list keeps its previous state. */
    }
  };

  return (
    <ConfirmationDialogController<GatewayAliasRow>
      title="Delete model alias"
      text={(alias) =>
        `Delete “${alias.name}”? Clients using this alias will immediately receive errors.`
      }
      confirmLabel="Delete alias"
      variant="destructive"
      loading={remove.isPending}
      error={remove.error?.message}
      onAfterDismiss={remove.reset}
      onConfirm={async (alias) => {
        await remove.mutateAsync({ orgId: organizationId, id: alias.id });
        capture("ai_gateway:alias_deleted", { orgId: organizationId });
        await invalidateAliases();
      }}
    >
      {({ openDialog: openDeleteDialog }) =>
        connectionsQuery.isPending ? null : (
          <AliasDialogController
            organizationId={organizationId}
            connections={connectionsQuery.data}
            onSaved={invalidateAliases}
          >
            {({ openDialog }) => (
              <SettingsTable<GatewayAliasRow>
                tableName="gatewayAliases"
                columns={columns}
                data={data}
                actions={(row) => [
                  ...(canManage
                    ? [
                        {
                          id: "toggle",
                          type: "item" as const,
                          title:
                            row.status === "ENABLED"
                              ? "Disable alias"
                              : "Enable alias",
                          onClick: () => toggleStatus(row),
                        },
                        {
                          id: "edit",
                          type: "item" as const,
                          title: "Edit alias",
                          onClick: () => openDialog(row),
                        },
                        {
                          id: "delete",
                          type: "item" as const,
                          title: "Delete alias",
                          icon: Trash2,
                          variant: "destructive" as const,
                          onClick: () => openDeleteDialog(row),
                        },
                      ]
                    : []),
                ]}
                actionsDisabledReason={
                  canManage
                    ? undefined
                    : "You need gateway management access to edit aliases."
                }
                toolbarActions={
                  canManage
                    ? [
                        {
                          id: "create-alias",
                          label: "New alias",
                          icon: <Plus size="base" aria-hidden="true" />,
                          onClick: () => openDialog(null),
                        },
                      ]
                    : []
                }
                noResultsMessage={
                  hasNoAliases
                    ? "No model aliases created."
                    : "No model aliases match the current search."
                }
                emptyState={
                  hasNoAliases ? (
                    <GatewaySetupEmptyState
                      icon={Plus}
                      title="No model aliases yet"
                      description="Create an alias to give clients a stable model name that maps to ordered provider model targets."
                      actions={
                        canManage
                          ? [
                              {
                                type: "button" as const,
                                label: "New alias",
                                onClick: () => openDialog(null),
                              },
                            ]
                          : []
                      }
                    />
                  ) : undefined
                }
              />
            )}
          </AliasDialogController>
        )
      }
    </ConfirmationDialogController>
  );
}

function AliasesSkeleton() {
  return (
    <div className="flex flex-col gap-4">
      <Skeleton className="h-7 w-48" />
      <Skeleton className="h-48 w-full" />
    </div>
  );
}
