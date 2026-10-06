// The configured agent: Chip runs the agent loop, FX runs the model call.
//
// Provider settings are runtime configuration, read from the environment the
// configured launcher inherits when a model step starts -- never at build time
// and never from this file, the formula, or the release artifact:
//
//   FX_BASE_URL               OpenAI-compatible API prefix (required)
//   FX_MODEL                  model id sent to that endpoint (required)
//   FX_API_KEY_ENV            name of the variable holding a bearer token (optional;
//                             FX reads it natively, so the key never enters JavaScript)
//   FX_CONTEXT_WINDOW_TOKENS  the model's context window (default 32000)
//
// Without FX_BASE_URL and FX_MODEL, Chip still starts and accepts sessions; each
// turn fails with the message below instead of falling back to another provider.
import { defineAgent, defineDynamic } from "@appport/chip";
import { fx } from "@appport/chip/models/fx";
import { createFxModel } from "@appport/fx";

function setting(name: string): string {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(
      `compute-configured: ${name} is not set. Configure the model provider with FX_BASE_URL and FX_MODEL (and FX_API_KEY_ENV for an endpoint that needs a bearer token).`,
    );
  }
  return value;
}

let selected: { key: string; model: ReturnType<typeof fx> } | undefined;

async function fxModel() {
  const config = {
    baseUrl: setting("FX_BASE_URL"),
    model: setting("FX_MODEL"),
    apiKeyEnv: process.env.FX_API_KEY_ENV?.trim() || undefined,
  };
  const key = JSON.stringify(config);
  if (selected?.key !== key) selected = { key, model: fx(await createFxModel(config)) };
  return selected.model;
}

function contextWindowTokens(): number {
  const tokens = Number(process.env.FX_CONTEXT_WINDOW_TOKENS ?? 32_000);
  if (!Number.isInteger(tokens) || tokens <= 0) {
    throw new Error("compute-configured: FX_CONTEXT_WINDOW_TOKENS must be a positive integer.");
  }
  return tokens;
}

export default defineAgent({
  // A live LanguageModel may only be selected per step; FX models are not in
  // any Gateway catalog, so the context window is always supplied.
  model: defineDynamic({
    events: {
      "step.started": async () => ({
        model: await fxModel(),
        modelContextWindowTokens: contextWindowTokens(),
      }),
    },
  }),
  // FX loads its native addon relative to its own installed package.
  build: { externalDependencies: ["@appport/fx"] },
});
