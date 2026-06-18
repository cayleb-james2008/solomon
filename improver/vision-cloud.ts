// Registers a vision-capable model (Ollama Cloud) as a Pi provider for Solomon's
// visual E2E review agent. The RSI coder loop uses maki-cloud.ts (text-only);
// this heavier vision model is used ONLY by the visual reviewer (visual_review.py).
//
// apiKey resolves via Pi's resolveConfigValue: a bare env-var NAME is replaced by
// process.env[NAME]. So the value MUST be the literal name "OLLAMA_API_KEY".
// The model id is taken from RSI_VISION_MODEL so the runner registers exactly
// the operator-chosen vision model.
export default function (pi: any) {
  pi.registerProvider("vision-cloud", {
    baseUrl: process.env.OLLAMA_BASE_URL || "https://ollama.com/v1",
    apiKey: "OLLAMA_API_KEY",
    api: "openai-completions",
    models: [
      {
        id: process.env.RSI_VISION_MODEL || "qwen/qwen2.5-vl-72b",
        name: "Vision Reviewer",
        reasoning: !!(process.env.RSI_REASONING && process.env.RSI_REASONING !== "off"),
        input: ["text", "image"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 128000,
        maxTokens: 8192,
      },
    ],
  });
}