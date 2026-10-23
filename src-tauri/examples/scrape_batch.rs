//! 临时批刮：用真实 Cookie 走 MetadataService 补齐待刮条目（用完即删）。
use std::path::PathBuf;
use std::sync::Mutex;

use cinema_adapters::javdb::JavdbProvider;
use cinema_app::metadata_service::{MetadataService, MetadataStore};
use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    ItemId, MediaItem, ScrapedComment, ScrapedImage, ScrapeState, SourceId,
};
use cinema_domain::ports::javdb::{JavdbClient, JavdbConfig};
use cinema_infra::db::Database;
use cinema_infra::error::InfraError;
use cinema_infra::repo::{MediaItemRepo, MetadataRepo};
use cinema_infra::subtitle_repo::SettingRepo;
use cinema_infra::tag_repo::{ItemTagRepo, SearchIndexRepo};

fn port_err(err: InfraError) -> PortError {
    match err {
        InfraError::Domain(p) => p,
        other => PortError::Backend(other.to_string()),
    }
}

fn safe_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else {
        "webp"
    }
}

struct Store {
    db: Mutex<Database>,
    poster_dir: PathBuf,
}

impl MetadataStore for Store {
    fn item(&self, id: &ItemId) -> Result<MediaItem> {
        MediaItemRepo::get(self.db.lock().expect("db").conn(), id)
            .map_err(port_err)?
            .ok_or_else(|| PortError::NotFound(format!("条目不存在：{}", id.0)))
    }
    fn save_scraped(&self, item: &MediaItem) -> Result<()> {
        MediaItemRepo::save_scraped(self.db.lock().expect("db").conn(), item).map_err(port_err)
    }
    fn set_scrape_state(&self, id: &ItemId, state: ScrapeState, error: Option<&str>) -> Result<()> {
        MediaItemRepo::set_scrape_state(self.db.lock().expect("db").conn(), id, state, error)
            .map_err(port_err)
    }
    fn pending_ids(&self, source_id: Option<&SourceId>) -> Result<Vec<ItemId>> {
        MediaItemRepo::ids_pending_scrape(self.db.lock().expect("db").conn(), source_id)
            .map_err(port_err)
    }
    fn reset_running(&self) -> Result<usize> {
        MediaItemRepo::reset_running_scrapes(self.db.lock().expect("db").conn()).map_err(port_err)
    }
    fn replace_screenshots(&self, id: &ItemId, shots: &[ScrapedImage]) -> Result<()> {
        MetadataRepo::replace_screenshots(self.db.lock().expect("db").conn(), id, shots)
            .map_err(port_err)
    }
    fn replace_comments(&self, id: &ItemId, comments: &[ScrapedComment]) -> Result<()> {
        MetadataRepo::replace_comments(self.db.lock().expect("db").conn(), id, comments)
            .map_err(port_err)
    }
    fn replace_metadata_tags(
        &self,
        id: &ItemId,
        dimension_id: &str,
        values: &[String],
    ) -> Result<()> {
        ItemTagRepo::replace_metadata(self.db.lock().expect("db").conn(), id, dimension_id, values)
            .map_err(port_err)
    }
    fn reindex_item(&self, id: &ItemId) -> Result<()> {
        SearchIndexRepo::reindex_item(self.db.lock().expect("db").conn(), id).map_err(port_err)
    }
    fn write_poster(&self, id: &ItemId, bytes: &[u8]) -> Result<String> {
        std::fs::create_dir_all(&self.poster_dir).map_err(|e| port_err(InfraError::Io(e)))?;
        let name = format!("{}.{}", safe_name(&id.0), image_ext(bytes));
        std::fs::write(self.poster_dir.join(&name), bytes)
            .map_err(|e| port_err(InfraError::Io(e)))?;
        Ok(name)
    }
    fn write_screenshot(&self, _id: &ItemId, file_name: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.poster_dir.parent().unwrap().join("shots");
        std::fs::create_dir_all(&dir).map_err(|e| port_err(InfraError::Io(e)))?;
        std::fs::write(dir.join(safe_name(file_name)), bytes)
            .map_err(|e| port_err(InfraError::Io(e)))
    }
    fn trailer_dest(&self, id: &ItemId, ext: &str) -> Result<PathBuf> {
        let dir = self.poster_dir.parent().unwrap().join("trailers");
        std::fs::create_dir_all(&dir).map_err(|e| port_err(InfraError::Io(e)))?;
        let ext = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
        Ok(dir.join(format!("{}.{}", safe_name(&id.0), ext)))
    }
}

fn main() {
    let limit: usize = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let data = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let root = std::path::Path::new(&data).join("Cinema");
    let db = Database::open(&root.join("cinema.db")).expect("打开数据库");
    let base = SettingRepo::get(db.conn(), "javdb.base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://javdb575.com".into());
    let cookie = SettingRepo::get(db.conn(), "javdb.cookie")
        .ok()
        .flatten()
        .unwrap_or_default();
    let config = JavdbConfig { base_url: base, cookie };
    let client = JavdbProvider::new();
    if !config.has_cookie() {
        println!("没有保存 Cookie，先配置再跑");
        return;
    }
    match client.login_state(&config) {
        Ok(state) => println!("登录态 = {state:?}"),
        Err(err) => println!("登录态检查失败 = {err:?}"),
    }

    let store = Store {
        db: Mutex::new(db),
        poster_dir: root.join("posters"),
    };
    let service = MetadataService::new(&client as &dyn JavdbClient, &store);
    let pending = store.pending_ids(None).unwrap_or_default();
    let take = limit.min(pending.len());
    println!("待刮 {} 条，本次执行 {}", pending.len(), take);

    let (mut ok, mut failed) = (0usize, 0usize);
    let mut streak = 0usize;
    for (idx, id) in pending.iter().take(take).enumerate() {
        match service.scrape_one(id, &config, false) {
            Ok(outcome) => {
                ok += 1;
                streak = 0;
                println!(
                    "[{}/{}] ok state={:?} comments={} shots={} trailer_saved={}",
                    idx + 1,
                    take,
                    outcome.state,
                    outcome.comments,
                    outcome.screenshots,
                    outcome.trailer_saved
                );
            }
            Err(err) => {
                failed += 1;
                streak += 1;
                println!("[{}/{}] fail {err:?}", idx + 1, take);
                if streak >= 5 {
                    println!("连续失败 5 次，提前停止（可能被限流）");
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    println!("完成：成功 {ok}，失败 {failed}");
}
