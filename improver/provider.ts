// Single parameterized Pi provider shim for Solomon's RSI loop. Replaces the three
// near-identical shims (maki-cloud.ts / openrouter.ts / vision-cloud.ts). The runner picks
// which provider to register via the RSI_PROVIDER env var (default "maki-cloud"); the model id
// comes from RSI_MODEL (or RSI_VISION_MODEL for the vision reviewer), reasoning from RSI_REASONING.
//
// apiKey resolves via Pi's resolveConfigValue: a bare env-var NAME is replaced by process.env[NAME],
// so the value MUST be the literal name (e.g. "OLLAMA_API_KEY", NOT "$OLLAMA_API_KEY"). The runner
// loads Solomon/.env before launching Pi so the key is set.
const PROVIDERS: Record<string, {
  baseUrl: string; apiKey: string; defaultModel: string; name: string;
  input: string[]; contextWindow: number;
}> = {
  "maki-cloud": {
    baseUrl: process.env.OLLAMA_BASE_URL || "https://ollama.com/v1",
    apiKey: "OLLAMA_API_KEY",
    defaultModel: "kimi-k2.7-code",
    name: "Kimi K2.7 Code",
    input: ["text"],
    contextWindow: 256000,
  },
  "openrouter": {
    baseUrl: "https://openrouter.ai/api/v1",
    apiKey: "OPENROUTER_API_KEY",
    defaultModel: "qwen/qwen3-coder",
    name: "",  // fall back to the model id as the display name
    input: ["text"],
    contextWindow: 200000,
  },
  "vision-cloud": {
    baseUrl: process.env.OLLAMA_BASE_URL || "https://ollama.com/v1",
    apiKey: "OLLAMA_API_KEY",
    defaultModel: "qwen/qwen2.5-vl-72b",
    name: "Vision Reviewer",
    input: ["text", "image"],
    contextWindow: 128000,
  },
};

export default function (pi: any) {
  const which = process.env.RSI_PROVIDER || "maki-cloud";
  const cfg = PROVIDERS[which] || PROVIDERS["maki-cloud"];
  const modelId =
    (which === "vision-cloud"
      ? process.env.RSI_VISION_MODEL || process.env.RSI_MODEL
      : process.env.RSI_MODEL) || cfg.defaultModel;
  pi.registerProvider(which, {
    baseUrl: cfg.baseUrl,
    apiKey: cfg.apiKey,
    api: "openai-completions",
    models: [
      {
        id: modelId,
        name: cfg.name || modelId,
        reasoning: !!(process.env.RSI_REASONING && process.env.RSI_REASONING !== "off"),
        input: cfg.input,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: cfg.contextWindow,
        maxTokens: 8192,
      },
    ],
  });
}
