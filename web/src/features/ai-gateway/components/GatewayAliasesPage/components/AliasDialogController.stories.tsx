import type { ComponentProps } from "react";
import { fn } from "storybook/test";

import preview from "@/.storybook/preview";
import { AliasDialogController } from "./AliasDialogController";

const meta = preview.meta({ component: AliasDialogController });

const connections = [
  {
    id: "conn-openai",
    name: "OpenAI production",
    provider: "OPENAI",
    allowedModels: ["gpt-4.1", "gpt-4.1-mini"],
    supportedRoutes: [],
  },
  {
    id: "conn-anthropic",
    name: "Anthropic",
    provider: "ANTHROPIC",
    allowedModels: [],
    supportedRoutes: [],
  },
] satisfies ComponentProps<typeof AliasDialogController>["connections"];

const onSaved = fn(async () => {});

export const NewAlias = meta.story({
  name: "(Test) New alias dialog opens",
  args: {
    organizationId: "org-1",
    connections,
    onSaved,
    children: ({ openDialog }) => (
      <button type="button" onClick={() => openDialog(null)}>
        New alias
      </button>
    ),
  },
  play: async ({ canvas, userEvent }) => {
    await userEvent.click(canvas.getByRole("button", { name: "New alias" }));
    await canvas.findByRole("dialog");
  },
});

export const EditAlias = meta.story({
  name: "(Test) Edit alias dialog opens with existing targets",
  args: {
    organizationId: "org-1",
    connections,
    onSaved,
    children: ({ openDialog }) => (
      <button
        type="button"
        onClick={() =>
          openDialog({
            id: "alias-1",
            name: "gpt-fast",
            status: "ENABLED",
            updatedAt: new Date("2026-01-01T00:00:00.000Z"),
            targets: [
              {
                id: "t-1",
                connectionId: "conn-openai",
                connectionName: "OpenAI production",
                providerModel: "gpt-4.1",
                position: 0,
              },
              {
                id: "t-2",
                connectionId: "conn-anthropic",
                connectionName: "Anthropic",
                providerModel: "claude-sonnet-4",
                position: 1,
              },
            ],
          })
        }
      >
        Edit alias
      </button>
    ),
  },
  play: async ({ canvas, userEvent }) => {
    await userEvent.click(canvas.getByRole("button", { name: "Edit alias" }));
    await canvas.findByRole("dialog");
  },
});
