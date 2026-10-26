"""全局配置加载器 - 读取 config/settings.yaml 并提供单例访问。"""
from __future__ import annotations

import os
from functools import lru_cache
from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel, Field

PROJECT_ROOT = Path(__file__).resolve().parent.parent
CONFIG_PATH = PROJECT_ROOT / "config" / "settings.yaml"


class DatabaseConfig(BaseModel):
    url: str
    echo: bool = False
    pool_size: int = 10
    max_overflow: int = 20


class CrawlConfig(BaseModel):
    default_rate_limit_seconds: int = 3
    random_jitter_range: list[int] = Field(default_factory=lambda: [1, 3])
    retry_times: int = 3
    retry_backoff: int = 2
    timeout: int = 30
    concurrent_requests: int = 4
    incremental_stop_threshold: int = 5
    max_pages_per_run: int = 50


class ProxyConfig(BaseModel):
    enabled: bool = False
    pool_url: str = ""
    refresh_interval: int = 300


class StorageConfig(BaseModel):
    base_dir: str = "./data"
    attachments_dir: str = "./data/attachments"
    snapshots_dir: str = "./data/snapshots"


class TaggingConfig(BaseModel):
    similarity_threshold: float = 0.85
    llm_confidence_threshold: float = 0.6
    enable_llm: bool = False


class SchedulerConfig(BaseModel):
    incremental_interval_hours: int = 6
    full_calibration_day: str = "sun"
    full_calibration_hour: int = 2


class EmailAlert(BaseModel):
    enabled: bool = False
    smtp_host: str = ""
    smtp_port: int = 465
    username: str = ""
    password: str = ""
    from_addr: str = ""
    to_addrs: list[str] = Field(default_factory=list)


class WecomAlert(BaseModel):
    enabled: bool = False
    webhook_url: str = ""


class AlertConfig(BaseModel):
    enabled: bool = True
    email: EmailAlert = Field(default_factory=EmailAlert)
    wecom: WecomAlert = Field(default_factory=WecomAlert)
    log_only: bool = True


class ApiConfig(BaseModel):
    host: str = "0.0.0.0"
    port: int = 8000
    page_size_default: int = 20
    page_size_max: int = 100


class Settings(BaseModel):
    database: DatabaseConfig
    crawl: CrawlConfig
    user_agents: list[str] = Field(default_factory=list)
    proxy: ProxyConfig = Field(default_factory=ProxyConfig)
    storage: StorageConfig = Field(default_factory=StorageConfig)
    tagging: TaggingConfig = Field(default_factory=TaggingConfig)
    scheduler: SchedulerConfig = Field(default_factory=SchedulerConfig)
    alert: AlertConfig = Field(default_factory=AlertConfig)
    api: ApiConfig = Field(default_factory=ApiConfig)


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    """加载并缓存全局配置。"""
    env_path = CONFIG_PATH
    if not env_path.exists():
        raise FileNotFoundError(f"配置文件不存在: {env_path}")
    with open(env_path, "r", encoding="utf-8") as f:
        raw: dict[str, Any] = yaml.safe_load(f)
    # 环境变量覆盖
    if db_url := os.getenv("DATABASE_URL"):
        raw.setdefault("database", {})["url"] = db_url
    return Settings(**raw)
