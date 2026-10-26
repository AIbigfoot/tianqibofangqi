"""项目串联：按项目编号归一化 + 单位 + 标题关键词 串联为 project 主档。"""
from __future__ import annotations

from sqlalchemy import select
from sqlalchemy.orm import Session

from db.models import Notice, Project
from parsers.text import normalize_project_no


def get_or_create_project(
    session: Session,
    project_no: str | None,
    project_name: str | None,
    procurement_unit: str | None,
    region: str | None = None,
) -> Project | None:
    """按归一化项目编号获取或创建 project 主档。"""
    norm_no = normalize_project_no(project_no)
    if not norm_no:
        return None
    stmt = select(Project).where(Project.project_no == norm_no)
    project = session.execute(stmt).scalar_one_or_none()
    if project:
        # 更新可能变更的字段
        if project_name and not project.project_name:
            project.project_name = project_name
        if procurement_unit and not project.procurement_unit:
            project.procurement_unit = procurement_unit
        if region and not project.region:
            project.region = region
        return project
    project = Project(
        project_no=norm_no,
        project_name=project_name,
        procurement_unit=procurement_unit,
        region=region,
        status="bidding",
    )
    session.add(project)
    session.flush()
    return project
