// Registers Kimi K2.7 (Ollama Cloud) as a Pi provider for Maki's self-improvement
// loop (rsi/run_improver.py). The in-app manga assistant stays on the local model
// (maki-local.ts); this heavier coder model is used ONLY by the RSI agent.
//
// apiKey resolves via Pi's resolveConfigValue: a bare env-var NAME is replaced by
// process.env[NAME]. So the value MUST be the literal name "OLLAMA_API_KEY" (NOT
// "$OLLAMA_API_KEY"). The runner loads rsi/.env before launching Pi so the key is set.
// Provider name "maki-cloud" is deliberately distinct from any global "ollama-cloud"
// extension so the two never collide if both happen to be present.
export default function (pi: any) {
  pi.registerProvider("maki-cloud", {
    baseUrl: process.env.OLLAMA_BASE_URL || "https://ollama.com/v1",
    apiKey: "OLLAMA_API_KEY",
    api: "openai-completions",
    models: [
      {
        id: process.env.RSI_MODEL || "kimi-k2.7-code",
        name: "Kimi K2.7 Code",
        reasoning: !!(process.env.RSI_REASONING && process.env.RSI_REASONING !== "off"),
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 256000,
        maxTokens: 8192,
      },
    ],
  });
}
