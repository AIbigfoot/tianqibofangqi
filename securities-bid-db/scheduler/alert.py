"""告警分发器：支持邮件 + 企微 Webhook，默认仅日志输出。"""
from __future__ import annotations

import json
import logging
import smtplib
from email.mime.text import MIMEText
from typing import Any

import requests

from config import get_settings

logger = logging.getLogger(__name__)
_settings = get_settings()


def send_alert(title: str, content: str) -> None:
    """发送告警，按配置分发到邮件/企微/日志。"""
    if not _settings.alert.enabled:
        return
    _log_alert(title, content)
    if _settings.alert.wecom.enabled and _settings.alert.wecom.webhook_url:
        _send_wecom(title, content)
    if _settings.alert.email.enabled:
        _send_email(title, content)
    if _settings.alert.log_only:
        logger.info("[ALERT] %s: %s", title, content)


def _log_alert(title: str, content: str) -> None:
    logger.warning("[ALERT] %s | %s", title, content[:500])


def _send_wecom(title: str, content: str) -> None:
    payload = {
        "msgtype": "markdown",
        "markdown": {"content": f"### {title}\n\n{content}"},
    }
    try:
        resp = requests.post(_settings.alert.wecom.webhook_url,
                             json=payload, timeout=10)
        if resp.status_code != 200:
            logger.error("Wecom alert failed: %s %s", resp.status_code, resp.text)
    except Exception as e:
        logger.error("Wecom alert exception: %s", e)


def _send_email(title: str, content: str) -> None:
    ec = _settings.alert.email
    if not ec.to_addrs:
        return
    msg = MIMEText(content, "plain", "utf-8")
    msg["Subject"] = title
    msg["From"] = ec.from_addr
    msg["To"] = ", ".join(ec.to_addrs)
    try:
        with smtplib.SMTP_SSL(ec.smtp_host, ec.smtp_port) as s:
            s.login(ec.username, ec.password)
            s.sendmail(ec.from_addr, ec.to_addrs, msg.as_string())
    except Exception as e:
        logger.error("Email alert failed: %s", e)
