//! L1 元数据服务：把数据源抓取结果合并进本地库。
//!
//! 分工：
//! - **L2** 声明 [`JavdbClient`] 端口（要哪些数据）；
//! - **L3** 负责页面抓取与解析（`cinema_adapters::javdb`）；
//! - **本层** 负责「认番号 → 搜索 → 合并用户锁定 → 下载图片/预告片 → 落库」的编排；
//! - **组合根** 提供 [`MetadataStore`]（SQL 与文件落盘，不做流程判断）。
//!
//! 失败语义：刮削失败**不影响**条目播放与已有元数据，只把 `scrape_state` 置为
//! `Failed` 并记录 `scrape_error`，供界面显示与下次重试。

use std::path::PathBuf;

use cinema_domain::error::{PortError, Result};
use cinema_domain::javdb_code;
use cinema_domain::model::{
    ItemId, MediaItem, ScrapedComment, ScrapedImage, ScrapeState, SourceId, IdentifyState,
    MetaState,
};
use cinema_domain::ports::javdb::{JavdbClient, JavdbConfig, RemoteDetail, TrailerKind};
use cinema_domain::rules::{FieldLock, LockedField};
use cinema_domain::tagging::{
    year_label_from, DIM_ACTOR, DIM_CATEGORY, DIM_DIRECTOR, DIM_MAKER, DIM_SERIES, DIM_YEAR,
};

/// 落库与文件能力（由组合根实现）。
pub trait MetadataStore: Send + Sync {
    fn item(&self, id: &ItemId) -> Result<MediaItem>;
    fn save_scraped(&self, item: &MediaItem) -> Result<()>;
    fn set_scrape_state(
        &self,
        id: &ItemId,
        state: ScrapeState,
        error: Option<&str>,
    ) -> Result<()>;
    fn pending_ids(&self, source_id: Option<&SourceId>) -> Result<Vec<ItemId>>;
    fn reset_running(&self) -> Result<usize>;
    fn replace_screenshots(&self, id: &ItemId, shots: &[ScrapedImage]) -> Result<()>;
    fn replace_comments(&self, id: &ItemId, comments: &[ScrapedComment]) -> Result<()>;
    /// 覆盖某维度上来源为元数据的旧值（不动用户来源与锁定）。
    fn replace_metadata_tags(&self, id: &ItemId, dimension_id: &str, values: &[String])
        -> Result<()>;
    fn reindex_item(&self, id: &ItemId) -> Result<()>;
    /// 写封面，返回 `poster_id`（`posters/<poster_id>` 可读）。
    fn write_poster(&self, id: &ItemId, bytes: &[u8]) -> Result<String>;
    /// 写一张截图（`shots/<file_name>`）。
    fn write_screenshot(&self, id: &ItemId, file_name: &str, bytes: &[u8]) -> Result<()>;
    /// 预告片目标路径（目录不存在时由实现创建）。
    fn trailer_dest(&self, id: &ItemId, ext: &str) -> Result<PathBuf>;
}

/// 一次刮削的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrapeOutcome {
    pub item_id: String,
    pub state: ScrapeState,
    /// 抓到的评论条数。
    pub comments: usize,
    /// 抓到的截图张数。
    pub screenshots: usize,
    pub trailer_saved: bool,
}

/// 元数据服务。
pub struct MetadataService<'a> {
    client: &'a dyn JavdbClient,
    store: &'a dyn MetadataStore,
}

impl<'a> MetadataService<'a> {
    pub fn new(client: &'a dyn JavdbClient, store: &'a dyn MetadataStore) -> Self {
        Self { client, store }
    }

    /// 刮削一个条目。`force = false` 时已完成（`Done`）的条目直接跳过。
    pub fn scrape_one(
        &self,
        id: &ItemId,
        config: &JavdbConfig,
        force: bool,
    ) -> Result<ScrapeOutcome> {
        let item = self.store.item(id)?;
        if !force && item.scrape_state == ScrapeState::Done {
            return Ok(ScrapeOutcome {
                item_id: id.0.clone(),
                state: ScrapeState::Done,
                comments: 0,
                screenshots: 0,
                trailer_saved: item.trailer_file.is_some(),
            });
        }

        let codes = item
            .external_code
            .clone()
            .map(|code| vec![code])
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| javdb_code::candidates(item.rel_path.as_deref(), &item.title));
        if codes.is_empty() {
            let message = "未能从文件名识别番号，请重命名文件或手动指定";
            self.store
                .set_scrape_state(id, ScrapeState::NoCode, Some(message))?;
            return Ok(ScrapeOutcome {
                item_id: id.0.clone(),
                state: ScrapeState::NoCode,
                comments: 0,
                screenshots: 0,
                trailer_saved: false,
            });
        }

        self.store.set_scrape_state(id, ScrapeState::Running, None)?;
        match self.scrape_detail(&item, &codes, config) {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                let message = err.reason().to_owned();
                let _ = self
                    .store
                    .set_scrape_state(id, ScrapeState::Failed, Some(&message));
                Err(err)
            }
        }
    }

    fn scrape_detail(
        &self,
        item: &MediaItem,
        codes: &[String],
        config: &JavdbConfig,
    ) -> Result<ScrapeOutcome> {
        let mut hit = None;
        for code in codes {
            let hits = self.client.search(config, code)?;
            if let Some(found) = hits.into_iter().next() {
                hit = Some(found);
                break;
            }
        }
        let hit = hit.ok_or_else(|| {
            PortError::NotFound(format!("数据源没有收录番号 {}", codes.join(" / ")))
        })?;

        let detail = self.client.detail(config, &hit.uid)?;
        self.apply(&item.id, detail, config)
    }

    fn apply(
        &self,
        id: &ItemId,
        detail: RemoteDetail,
        config: &JavdbConfig,
    ) -> Result<ScrapeOutcome> {
        let mut item = self.store.item(id)?;
        let lock = FieldLock::from_csv(item.field_locks.as_deref().unwrap_or(""));

        // 标题 = 番号 + 标题（用户锁定的标题不覆盖）
        let full_title = compose_title(&detail.code, &detail.title);
        if !lock.is_locked(LockedField::Title) && !full_title.is_empty() {
            item.title = full_title;
        }
        if !detail.code.is_empty() {
            item.external_code = Some(detail.code.clone());
        }
        if !detail.title.is_empty() {
            item.title_origin = Some(detail.title.clone());
        }
        if detail.release_date.is_some() {
            item.release_date = detail.release_date.clone();
        }
        // 年份字段与发行日期保持同步，供标签与检索使用（日期优先解析前四位）。
        if item.release_date.is_some() {
            item.year = item
                .release_date
                .as_deref()
                .and_then(|date| date.trim().get(0..4))
                .and_then(|prefix| prefix.parse::<i32>().ok())
                .or(item.year);
        }
        if detail.duration_ms.is_some() {
            item.duration_ms = detail.duration_ms;
        }
        if let Some(rating) = detail.rating {
            item.rating = Some(rating as f32);
        }
        item.detail_url = Some(detail_url(&config.base_url, &detail.uid));
        item.scraped_at = Some(now_ms());
        item.scrape_state = ScrapeState::Done;
        item.scrape_error = None;
        item.identify_state = IdentifyState::Identified;
        item.meta_state = MetaState::Complete;

        // 封面：下载后才写入 poster_id（失败不影响其余字段）
        if !lock.is_locked(LockedField::Poster) {
            if let Some(url) = detail.cover_url.as_deref() {
                if let Ok(bytes) = self.client.fetch_bytes(config, url) {
                    if !bytes.is_empty() {
                        if let Ok(poster_id) = self.store.write_poster(id, &bytes) {
                            item.poster_id = Some(poster_id);
                        }
                    }
                }
            }
        }

        let mut shot_count = 0usize;
        let mut saved_shots = Vec::new();
        for shot in &detail.screenshots {
            let Some(url) = shot.source_url.as_deref() else {
                continue;
            };
            let Ok(bytes) = self.client.fetch_bytes(config, url) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            if self
                .store
                .write_screenshot(id, &shot.file_name, &bytes)
                .is_ok()
            {
                saved_shots.push(shot.clone());
                shot_count += 1;
            }
        }

        // 预告片：优先本地化；无法本地化时保留远端地址供在线播放。
        let mut trailer_saved = false;
        if let Some(trailer) = detail.trailer.as_ref() {
            item.trailer_url = Some(trailer.url.clone());
            let ext = match trailer.kind {
                TrailerKind::Hls => "ts",
                TrailerKind::Direct => extension_of(&trailer.url).unwrap_or("mp4"),
            };
            if let Ok(dest) = self.store.trailer_dest(id, ext) {
                if self.client.save_trailer(config, trailer, &dest).is_ok() {
                    if let Some(name) = dest.file_name().and_then(|n| n.to_str()) {
                        item.trailer_file = Some(name.to_owned());
                        item.trailer_fetched_at = Some(now_ms());
                        trailer_saved = true;
                    }
                }
            }
        }

        self.store.save_scraped(&item)?;
        self.store.replace_screenshots(id, &saved_shots)?;
        self.store.replace_comments(id, &detail.comments)?;

        if !lock.is_locked(LockedField::Tags) {
            let year_values: Vec<String> =
                year_label_from(item.release_date.as_deref(), item.year)
                    .into_iter()
                    .collect();
            let groups: [(&str, Vec<String>); 6] = [
                (DIM_CATEGORY, detail.categories.clone()),
                (DIM_SERIES, detail.series.clone().into_iter().collect()),
                (DIM_MAKER, detail.maker.clone().into_iter().collect()),
                (DIM_DIRECTOR, detail.director.clone().into_iter().collect()),
                (DIM_ACTOR, detail.actors.clone()),
                (DIM_YEAR, year_values),
            ];
            for (dimension, values) in groups {
                self.store.replace_metadata_tags(id, dimension, &values)?;
            }
        }
        self.store.reindex_item(id)?;

        Ok(ScrapeOutcome {
            item_id: id.0.clone(),
            state: ScrapeState::Done,
            comments: detail.comments.len(),
            screenshots: shot_count,
            trailer_saved,
        })
    }
}

/// 全名规则：`番号 + 空格 + 标题`（标题为空时只留番号）。
pub fn compose_title(code: &str, title: &str) -> String {
    let code = code.trim();
    let title = title.trim();
    match (code.is_empty(), title.is_empty()) {
        (true, _) => title.to_owned(),
        (_, true) => code.to_owned(),
        _ => {
            if title.starts_with(code) {
                title.to_owned()
            } else {
                format!("{code} {title}")
            }
        }
    }
}

fn detail_url(base_url: &str, uid: &str) -> String {
    format!("{}/v/{}", base_url.trim_end_matches('/'), uid)
}

fn extension_of(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, ext)| ext).filter(|ext| {
        !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_code_plus_title() {
        assert_eq!(compose_title("SSIS-001", "某标题"), "SSIS-001 某标题");
        assert_eq!(compose_title("SSIS-001", ""), "SSIS-001");
        assert_eq!(compose_title("", "某标题"), "某标题");
        assert_eq!(compose_title("SSIS-001", "SSIS-001 某标题"), "SSIS-001 某标题");
    }

    #[test]
    fn extension_is_taken_from_url_without_query() {
        assert_eq!(extension_of("https://x/a/b.mp4?t=1"), Some("mp4"));
        assert_eq!(extension_of("https://x/a/b.m3u8"), Some("m3u8"));
        assert_eq!(extension_of("https://x/a/b"), None);
    }
}
