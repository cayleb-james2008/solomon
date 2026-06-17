// Registers an OpenRouter model as a Pi provider for the RSI self-improvement
// loop (run_improver.py --provider openrouter). Mirrors maki-cloud.ts.
//
// apiKey resolves via Pi's resolveConfigValue: a bare env-var NAME is replaced by
// process.env[NAME]. So the value MUST be the literal name "OPENROUTER_API_KEY"
// (NOT "$OPENROUTER_API_KEY"). The runner loads rsi/.env before launching Pi so
// the key is set. The model id is taken from RSI_MODEL so the runner registers
// exactly the operator-chosen model.
export default function (pi: any) {
  pi.registerProvider("openrouter", {
    baseUrl: "https://openrouter.ai/api/v1",
    apiKey: "OPENROUTER_API_KEY",
    api: "openai-completions",
    models: [
      {
        id: process.env.RSI_MODEL || "qwen/qwen3-coder",
        name: process.env.RSI_MODEL || "qwen/qwen3-coder",
        reasoning: !!(process.env.RSI_REASONING && process.env.RSI_REASONING !== "off"),
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 200000,
        maxTokens: 8192,
      },
    ],
  });
}
