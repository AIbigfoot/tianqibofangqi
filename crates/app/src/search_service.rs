//! L1 检索与筛选编排（PRD FR-02 检索区、FR-03 标签检索）。
//!
//! 本层**不实现**「同维度 OR、跨维度 AND」——那是 L2 唯一的一份实现
//! （[`matches_filters`]）。本层只做三件事：把「文本命中 ∩ 标签筛选」组合起来、
//! 按选定模式排序、把维度清单转交给界面。
//!
//! 因此新增一个标签维度不需要改动本文件的任何一行（硬约束 C6）。

use std::collections::{BTreeMap, HashSet};

use cinema_domain::error::Result;
use cinema_domain::model::{ItemId, MediaItem};
use cinema_domain::ports::tag_dimension::TagDimensionDef;
use cinema_domain::rules::{
    compare_items, matches_filters, FilterSelection, ItemTags, SortKey, SortMode,
};
use cinema_domain::tagging::TagUsage;

/// 一次检索的输入。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// 文本查询（匹配标题 / 简介 / 标签名）；空白表示不施加文本约束。
    pub text: String,
    /// 维度筛选：`dimension_id -> 选中取值`。空选中表示该维度不限。
    pub filters: FilterSelection,
    /// 排序模式。
    pub sort: SortMode,
}

/// 某条目在检索结果中需要的进度事实。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressState {
    /// 是否已达「已看」。
    pub watched: bool,
    /// 是否存在有效进度。
    pub has_progress: bool,
    /// 最近一次进度更新时间（`playback_state.updated_at`）；从未播放为 `None`。
    pub last_watched_at: Option<i64>,
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
}

/// 检索结果中的一条。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub item: MediaItem,
    /// 该条目的标签取值，界面前端据此展示命中原因，无需再查一次库。
    pub tags: ItemTags,
}

/// L1 检索所需的读取能力（实现由组合根提供）。
pub trait SearchStore: Send + Sync {
    /// 已登记的维度，按 `order` 升序。
    fn dimensions(&self) -> Result<Vec<TagDimensionDef>>;

    /// 某维度下的取值及使用频次（频次降序）。
    fn tags_of_dimension(&self, dimension_id: &str) -> Result<Vec<TagUsage>>;

    /// 全部条目。
    fn items(&self) -> Result<Vec<MediaItem>>;

    /// 全库标签：`item_id -> ItemTags`，供组合筛选一次读全量。
    fn all_item_tags(&self) -> Result<BTreeMap<String, ItemTags>>;

    /// 文本命中的条目 id。
    fn match_ids(&self, text: &str) -> Result<Vec<ItemId>>;

    /// 某条目的进度事实。
    fn progress_state(&self, item_id: &ItemId) -> Result<ProgressState>;
}

/// 检索编排。
pub struct SearchService;

impl SearchService {
    /// 已登记维度清单，供检索区渲染维度面板。
    pub fn dimensions(store: &dyn SearchStore) -> Result<Vec<TagDimensionDef>> {
        store.dimensions()
    }

    /// 某维度的取值清单（按频次降序），供检索区渲染标签。
    pub fn dimension_tags(store: &dyn SearchStore, dimension_id: &str) -> Result<Vec<TagUsage>> {
        store.tags_of_dimension(dimension_id)
    }

    /// 组合检索：文本命中 ∩ 维度筛选，再按 `query.sort` 排序。
    ///
    /// 文本为空时不施加文本约束；筛选中某维度为空选中时该维度不限。
    /// 排序在 L2 的 [`compare_items`] 内完成，同值条目由标题兜底，结果稳定。
    pub fn search(store: &dyn SearchStore, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let text_hit: Option<HashSet<String>> = if query.text.trim().is_empty() {
            None
        } else {
            Some(
                store
                    .match_ids(&query.text)?
                    .into_iter()
                    .map(|id| id.0)
                    .collect(),
            )
        };

        let all_tags = store.all_item_tags()?;
        let mut decorated: Vec<(SortKey, SearchHit)> = Vec::new();
        for item in store.items()? {
            if let Some(hit) = &text_hit {
                if !hit.contains(&item.id.0) {
                    continue;
                }
            }
            let tags = all_tags.get(&item.id.0).cloned().unwrap_or_default();
            if !matches_filters(&query.filters, &tags) {
                continue;
            }
            let key = sort_key(store, &item, query.sort)?;
            decorated.push((key, SearchHit { item, tags }));
        }

        decorated.sort_by(|a, b| compare_items(query.sort, &a.0, &b.0));
        Ok(decorated.into_iter().map(|(_, hit)| hit).collect())
    }
}

/// 构造排序键。只有「最近观看」需要额外读进度，其余模式不产生多余查询。
fn sort_key(store: &dyn SearchStore, item: &MediaItem, sort: SortMode) -> Result<SortKey> {
    let last_watched_at = if sort == SortMode::LastWatched {
        store.progress_state(&item.id)?.last_watched_at
    } else {
        None
    };
    Ok(SortKey {
        added_at: item.created_at.unwrap_or(0),
        title: item.title.clone(),
        year: item.year,
        rating: item.rating,
        duration_ms: item.duration_ms,
        last_watched_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use cinema_domain::ports::tag_dimension::{Selector, TagSource};
    use cinema_domain::tagging::{DIM_DURATION, DIM_GENRE, DURATION_SHORT, DURATION_FEATURE};

    /// 内存假仓储：模拟 `media_item` / `item_tag` / `playback_state` 三张表。
    #[derive(Default)]
    struct FakeStore {
        dims: Vec<TagDimensionDef>,
        items: Vec<MediaItem>,
        tags: BTreeMap<String, ItemTags>,
        usage: BTreeMap<String, Vec<TagUsage>>,
        progress: BTreeMap<String, ProgressState>,
    }

    impl FakeStore {
        fn dimension(id: &str, label: &str, order: u32) -> TagDimensionDef {
            TagDimensionDef {
                id: id.to_owned(),
                label: label.to_owned(),
                selector: Selector::Multi,
                source: TagSource::Metadata,
                expand: "value".to_owned(),
                order,
                editable: true,
                lockable: true,
                derive_rule: None,
            }
        }

        fn item(id: &str, title: &str, added_at: i64) -> MediaItem {
            let mut item = MediaItem::pending_fill(id, "src-1", title);
            item.created_at = Some(added_at);
            item
        }

        fn tag_of(mut self, item_id: &str, dimension: &str, value: &str) -> Self {
            self.tags
                .entry(item_id.to_owned())
                .or_default()
                .entry(dimension.to_owned())
                .or_insert_with(Vec::new)
                .push(value.to_owned());
            self
        }
    }

    impl SearchStore for FakeStore {
        fn dimensions(&self) -> Result<Vec<TagDimensionDef>> {
            Ok(self.dims.clone())
        }

        fn tags_of_dimension(&self, dimension_id: &str) -> Result<Vec<TagUsage>> {
            Ok(self.usage.get(dimension_id).cloned().unwrap_or_default())
        }

        fn items(&self) -> Result<Vec<MediaItem>> {
            Ok(self.items.clone())
        }

        fn all_item_tags(&self) -> Result<BTreeMap<String, ItemTags>> {
            Ok(self.tags.clone())
        }

        fn match_ids(&self, text: &str) -> Result<Vec<ItemId>> {
            // 模拟 FTS 的「标题或标签名包含查询词」
            let needle = text.trim();
            let mut out = Vec::new();
            for item in &self.items {
                let in_title = !needle.is_empty() && item.title.contains(needle);
                let in_tags = self
                    .tags
                    .get(&item.id.0)
                    .map(|tags| {
                        tags.values()
                            .flatten()
                            .any(|value| value.contains(needle))
                    })
                    .unwrap_or(false);
                if in_title || in_tags {
                    out.push(item.id.clone());
                }
            }
            Ok(out)
        }

        fn progress_state(&self, item_id: &ItemId) -> Result<ProgressState> {
            Ok(self.progress.get(&item_id.0).copied().unwrap_or_default())
        }
    }

    fn store() -> FakeStore {
        FakeStore {
            dims: vec![
                FakeStore::dimension(DIM_GENRE, "内容类型", 1),
                FakeStore::dimension(DIM_DURATION, "时长区间", 4),
            ],
            items: vec![
                FakeStore::item("it-a", "银翼杀手", 100),
                FakeStore::item("it-b", "银翼杀手 2049", 200),
                FakeStore::item("it-c", "别的电影", 300),
            ],
            ..Default::default()
        }
        .tag_of("it-a", DIM_GENRE, "动作")
        .tag_of("it-a", DIM_DURATION, DURATION_SHORT)
        .tag_of("it-b", DIM_GENRE, "科幻")
        .tag_of("it-b", DIM_DURATION, DURATION_SHORT)
        .tag_of("it-c", DIM_GENRE, "动作")
        .tag_of("it-c", DIM_DURATION, DURATION_FEATURE)
    }

    fn ids(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.item.id.0.clone()).collect()
    }

    #[test]
    fn same_dimension_or_and_across_dimension_and() {
        let store = store();
        let mut filters = FilterSelection::new();
        filters.insert(DIM_GENRE.into(), vec!["动作".into(), "科幻".into()]);
        filters.insert(DIM_DURATION.into(), vec![DURATION_SHORT.into()]);

        let hits = SearchService::search(
            &store,
            &SearchQuery {
                filters,
                ..SearchQuery::default()
            },
        )
        .expect("检索");

        // it-a、it-b 命中（同维度 OR）；it-c 为「动作」但时长不匹配（跨维度 AND）
        assert_eq!(ids(&hits), vec!["it-b", "it-a"], "默认按最近添加（新→旧）排列");
    }

    #[test]
    fn text_search_is_combined_with_tag_filters() {
        let store = store();
        let mut filters = FilterSelection::new();
        filters.insert(DIM_GENRE.into(), vec!["科幻".into()]);

        let hits = SearchService::search(
            &store,
            &SearchQuery {
                text: "银翼".into(),
                filters,
                ..SearchQuery::default()
            },
        )
        .expect("检索");

        assert_eq!(ids(&hits), vec!["it-b"], "文本命中后再按标签收窄");
    }

    #[test]
    fn empty_query_returns_whole_library() {
        let store = store();
        let hits = SearchService::search(&store, &SearchQuery::default()).expect("检索");
        assert_eq!(ids(&hits), vec!["it-c", "it-b", "it-a"]);
    }

    #[test]
    fn hits_carry_item_tags_for_display() {
        let store = store();
        let hits = SearchService::search(
            &store,
            &SearchQuery {
                filters: {
                    let mut f = FilterSelection::new();
                    f.insert(DIM_GENRE.into(), vec!["科幻".into()]);
                    f
                },
                ..SearchQuery::default()
            },
        )
        .expect("检索");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tags[DIM_GENRE], vec!["科幻".to_owned()]);
    }

    #[test]
    fn last_watched_sort_reads_progress_and_puts_never_watched_last() {
        let mut store = store();
        store.progress.insert(
            "it-a".into(),
            ProgressState {
                has_progress: true,
                last_watched_at: Some(500),
                ..Default::default()
            },
        );
        store.progress.insert(
            "it-b".into(),
            ProgressState {
                has_progress: true,
                last_watched_at: Some(900),
                ..Default::default()
            },
        );

        let hits = SearchService::search(
            &store,
            &SearchQuery {
                sort: SortMode::LastWatched,
                ..SearchQuery::default()
            },
        )
        .expect("检索");
        assert_eq!(ids(&hits), vec!["it-b", "it-a", "it-c"], "近→远，从未播放排末尾");
    }

    #[test]
    fn new_dimension_needs_no_search_code_change() {
        let mut store = store();
        // 「改配置」：多一条维度定义 + 数据多一行标签
        store.dims.push(FakeStore::dimension("mood", "心情", 99));
        store.tags.entry("it-c".into()).or_default().insert(
            "mood".into(),
            vec!["轻松".to_owned()],
        );
        store
            .usage
            .insert("mood".into(), vec![TagUsage { tag_id: "tg-1".into(), name: "轻松".into(), count: 1 }]);

        let mut filters = FilterSelection::new();
        filters.insert("mood".into(), vec!["轻松".into()]);
        let hits = SearchService::search(
            &store,
            &SearchQuery {
                filters,
                ..SearchQuery::default()
            },
        )
        .expect("检索");

        assert_eq!(ids(&hits), vec!["it-c"], "新维度天然获得同维度 OR / 跨维度 AND 语义");

        // 维度清单与取值清单同样无需改代码
        assert_eq!(SearchService::dimensions(&store).expect("维度").len(), 3);
        assert_eq!(
            SearchService::dimension_tags(&store, "mood").expect("取值")[0].name,
            "轻松"
        );
    }
}
