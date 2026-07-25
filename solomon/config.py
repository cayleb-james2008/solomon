"""Configuration — load .env into a typed dataclass."""
import os
import json
from dataclasses import dataclass, field
from pathlib import Path
from dotenv import load_dotenv


@dataclass
class Config:
    # LLM
    llm_base_url: str
    llm_api_key: str
    llm_model: str

    # VLM fallback
    vlm_base_url: str
    vlm_api_key: str
    vlm_model: str

    # Browser
    browser_profile: str
    browser_headless: bool
    browser_ua: str

    # Channels
    channel_freelance: bool
    channel_content: bool
    channel_microtask: bool

    # Safety
    auto_submit: bool

    # CEO loop
    max_cycles: int
    cycle_sleep: int

    # Notifications
    ntfy_topic: str

    # Paths
    project_root: Path = field(default_factory=lambda: Path.cwd())

    @classmethod
    def load(cls) -> "Config":
        load_dotenv(Path.cwd() / ".env")
        root = Path.cwd()
        return cls(
            llm_base_url=os.getenv("SOLOMON_LLM_BASE_URL", "https://open.bigmodel.cn/api/paas/v4"),
            llm_api_key=os.getenv("SOLOMON_LLM_API_KEY", ""),
            llm_model=os.getenv("SOLOMON_LLM_MODEL", "glm-4-flash"),
            vlm_base_url=os.getenv("SOLOMON_VLM_BASE_URL", "http://localhost:8012/v1"),
            vlm_api_key=os.getenv("SOLOMON_VLM_API_KEY", "sk-no-key-needed"),
            vlm_model=os.getenv("SOLOMON_VLM_MODEL", "MiniCPM-V-2.6"),
            browser_profile=os.getenv("SOLOMON_BROWSER_PROFILE", str(root / "runtime" / "browser-profile")),
            browser_headless=os.getenv("SOLOMON_BROWSER_HEADLESS", "false").lower() == "true",
            browser_ua=os.getenv("SOLOMON_BROWSER_UA", ""),
            channel_freelance=os.getenv("SOLOMON_CHANNEL_FREELANCE", "true").lower() == "true",
            channel_content=os.getenv("SOLOMON_CHANNEL_CONTENT", "true").lower() == "true",
            channel_microtask=os.getenv("SOLOMON_CHANNEL_MICROTASK", "true").lower() == "true",
            auto_submit=os.getenv("SOLOMON_AUTO_SUBMIT", "false").lower() == "true",
            max_cycles=int(os.getenv("SOLOMON_MAX_CYCLES", "1")),
            cycle_sleep=int(os.getenv("SOLOMON_CYCLE_SLEEP", "300")),
            ntfy_topic=os.getenv("SOLOMON_NTFY_TOPIC", ""),
            project_root=root,
        )

    @property
    def runtime_dir(self) -> Path:
        d = self.project_root / "runtime"
        d.mkdir(parents=True, exist_ok=True)
        return d

    @property
    def revenue_ledger_path(self) -> Path:
        return self.runtime_dir / "revenue.jsonl"

    @property
    def proposals_dir(self) -> Path:
        d = self.runtime_dir / "proposals"
        d.mkdir(parents=True, exist_ok=True)
        return d

    @property
    def articles_dir(self) -> Path:
        d = self.runtime_dir / "articles"
        d.mkdir(parents=True, exist_ok=True)
        return d

    @property
    def logs_dir(self) -> Path:
        d = self.runtime_dir / "logs"
        d.mkdir(parents=True, exist_ok=True)
        return d
