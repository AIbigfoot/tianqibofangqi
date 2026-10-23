//! L2 实时字幕（PRD FR-05 / 架构文档 §6）的**纯逻辑**。
//!
//! 本模块只做三件事，全部无 IO：
//! 1. **会话状态机**（[`RealtimeState`]）——「初始化 / 运行中 / 已暂停 / 已保存 / 失败」；
//! 2. **会话配置推导**（[`session_cfg`]）——手动指定优先、目标语言默认简体中文、
//!    呈现方式决定是否翻译；
//! 3. **结果收集**（[`CueCollector`]）——只收 `Final` / `Translated`，归一为
//!    一条 `kind=Generated` 的 [`SubtitleTrack`]。
//!
//! 以下口径来自文档，不得在此重新定义：
//! - `Partial` 只是视觉占位，**不进入保存的轨道**（架构 §6「句界驱动输出」）；
//! - 水位超阈值必须降级（「仅原文」或暂停字幕），不无界缓冲（硬约束 C4）；
//! - 失败后给三个出口（加载外挂字幕 / 仅做原文听写 / 关闭实时字幕），
//!   播放与界面不受影响（PRD FR-05 / 架构 §10 M3 验收）。

use crate::model::{
    CueEvent, SessionCfg, SubtitleCue, SubtitleKind, SubtitleTrack, Watermark,
};
use crate::model::ItemId;

/// 目标译文语言默认值（PRD FR-05：默认简体中文）。
pub const DEFAULT_TARGET_LANG: &str = "zh";

// ---------------------------------------------------------------------------
// 会话状态机（PRD FR-05「状态」）
// ---------------------------------------------------------------------------

/// 实时字幕会话状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeState {
    /// 尚未开启（默认）。
    Idle,
    /// 正在启动本地引擎 / 子进程或建链远端。
    Initializing,
    /// 运行中：识别与（必要时）翻译正常产出。
    Running,
    /// 已暂停：挂起识别并**保留末句**，恢复后继续（PRD FR-05）。
    Paused,
    /// 已保存为字幕轨道（可再次开启）。
    Saved,
    /// 失败：给出三个出口，播放与界面不受影响（PRD FR-05 / 架构 §10）。
    Failed,
}

/// 驱动状态迁移的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeEvent {
    /// 用户开启实时字幕。
    Start,
    /// 引擎就绪。
    Ready,
    /// 暂停播放（挂起识别，保留末句）。
    Pause,
    /// 恢复播放（继续识别）。
    Resume,
    /// 结果已保存为字幕轨道。
    Saved,
    /// 引擎 / 子进程异常。
    Fail,
    /// 停止会话，释放子进程句柄。
    Stop,
    /// 失败后原地重启。
    Restart,
}

impl RealtimeState {
    /// 状态是否处于「占用识别子进程」的活动态。
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RealtimeState::Initializing | RealtimeState::Running | RealtimeState::Paused
        )
    }

    /// 迁移。非法迁移返回 `None`——调用方应保持原状态，不得强行推进。
    pub fn on(self, event: RealtimeEvent) -> Option<RealtimeState> {
        use RealtimeEvent as E;
        use RealtimeState as S;
        match (self, event) {
            (S::Idle, E::Start) => Some(S::Initializing),
            (S::Initializing, E::Ready) => Some(S::Running),
            (S::Initializing, E::Fail) => Some(S::Failed),
            (S::Initializing, E::Stop) => Some(S::Idle),
            (S::Running, E::Pause) => Some(S::Paused),
            (S::Running, E::Saved) => Some(S::Saved),
            (S::Running, E::Fail) => Some(S::Failed),
            (S::Running, E::Stop) => Some(S::Idle),
            (S::Paused, E::Resume) => Some(S::Running),
            (S::Paused, E::Saved) => Some(S::Saved),
            (S::Paused, E::Fail) => Some(S::Failed),
            (S::Paused, E::Stop) => Some(S::Idle),
            (S::Failed, E::Restart) => Some(S::Initializing),
            (S::Failed, E::Stop) => Some(S::Idle),
            // 已保存后可再次开启；停止一律回到空闲
            (S::Saved, E::Start) => Some(S::Initializing),
            (S::Saved, E::Stop) => Some(S::Idle),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 呈现方式（PRD FR-05「原文+译文 / 仅译文 / 仅原文」）
// ---------------------------------------------------------------------------

/// 字幕呈现方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CueDisplay {
    /// 原文 + 译文两行（默认）。
    #[default]
    Bilingual,
    /// 仅译文。
    TranslationOnly,
    /// 仅原文。
    OriginOnly,
}

impl CueDisplay {
    /// 是否展示译文。
    pub fn shows_translation(self) -> bool {
        matches!(self, CueDisplay::Bilingual | CueDisplay::TranslationOnly)
    }

    /// 是否展示原文。
    pub fn shows_origin(self) -> bool {
        matches!(self, CueDisplay::Bilingual | CueDisplay::OriginOnly)
    }

    /// 界面标记 → 呈现方式；未知回落默认（双语），不猜测。
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "translation_only" => CueDisplay::TranslationOnly,
            "origin_only" => CueDisplay::OriginOnly,
            _ => CueDisplay::Bilingual,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            CueDisplay::Bilingual => "bilingual",
            CueDisplay::TranslationOnly => "translation_only",
            CueDisplay::OriginOnly => "origin_only",
        }
    }
}

// ---------------------------------------------------------------------------
// 会话配置推导
// ---------------------------------------------------------------------------

/// 由界面输入推导引擎会话配置。
///
/// 口径（PRD FR-05）：
/// - **手动指定优先**：用户给了原声语种就用它，否则 `None`（由引擎自动判定）；
/// - 目标语言缺省为简体中文（[`DEFAULT_TARGET_LANG`]）；
/// - 只有「需要展示译文」且「目标语言 ≠ 原声语种」时才开启翻译 ——
///   「仅原文」或「目标语言 = 原声」即**只听写不翻译**。
pub fn session_cfg(
    origin_lang: Option<&str>,
    target_lang: Option<&str>,
    display: CueDisplay,
) -> SessionCfg {
    let origin = normalize_lang(origin_lang);
    let target = normalize_lang(target_lang).or_else(|| Some(DEFAULT_TARGET_LANG.to_owned()));
    let same_lang = match (&origin, &target) {
        (Some(o), Some(t)) => o.eq_ignore_ascii_case(t),
        _ => false,
    };
    SessionCfg {
        origin_lang: origin,
        target_lang: target,
        translate: display.shows_translation() && !same_lang,
    }
}

fn normalize_lang(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// 水位降级（硬约束 C4）
// ---------------------------------------------------------------------------

/// 水位降级动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeAction {
    /// 无需降级。
    None,
    /// 降级为「仅原文」（关闭翻译）。
    OriginOnly,
    /// 暂停字幕（水位回落安全线后再恢复）。
    Pause,
}

/// 由水位与当前呈现方式决定降级动作。
///
/// - `High`：关闭翻译，只出原文；已经是「仅原文」时无需动作；
/// - `Critical`：暂停字幕，避免无界缓冲。
pub fn degrade_for(watermark: Watermark, display: CueDisplay) -> DegradeAction {
    match watermark {
        Watermark::Ok => DegradeAction::None,
        Watermark::High => {
            if display.shows_translation() {
                DegradeAction::OriginOnly
            } else {
                DegradeAction::None
            }
        }
        Watermark::Critical => DegradeAction::Pause,
    }
}

// ---------------------------------------------------------------------------
// 入口可见性（PRD FR-05）
// ---------------------------------------------------------------------------

/// 实时字幕入口是否出现在控制栏。
///
/// 口径：**仅当没有内嵌字幕、也没有外挂字幕**时出现。已生成实时字幕不算
/// 「可用字幕轨道」，不因它隐藏入口（用户可再次开启）。
pub fn should_offer_entry(has_embedded: bool, has_external: bool) -> bool {
    !has_embedded && !has_external
}

// ---------------------------------------------------------------------------
// 失败三出口（PRD FR-05）
// ---------------------------------------------------------------------------

/// 引擎失败后提供的三个出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureExit {
    /// 加载外挂字幕文件。
    LoadExternal,
    /// 仅做原文听写（不做翻译）。
    OriginDictation,
    /// 关闭实时字幕。
    Close,
}

impl FailureExit {
    /// 顺序与 PRD FR-05 一致。
    pub const ALL: [FailureExit; 3] = [
        FailureExit::LoadExternal,
        FailureExit::OriginDictation,
        FailureExit::Close,
    ];

    pub fn tag(self) -> &'static str {
        match self {
            FailureExit::LoadExternal => "load_external",
            FailureExit::OriginDictation => "origin_dictation",
            FailureExit::Close => "close",
        }
    }

    /// 界面标记 → 出口；未知返回 `None`（不猜测用户意图）。
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "load_external" => Some(FailureExit::LoadExternal),
            "origin_dictation" => Some(FailureExit::OriginDictation),
            "close" => Some(FailureExit::Close),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FailureExit::LoadExternal => "加载外挂字幕",
            FailureExit::OriginDictation => "仅做原文听写",
            FailureExit::Close => "关闭实时字幕",
        }
    }
}

// ---------------------------------------------------------------------------
// 结果收集（架构 §6「句界驱动输出」+「未保存结果保护」）
// ---------------------------------------------------------------------------

/// 把引擎事件流收集为一条可保存的轨道。
///
/// 只吸收 [`CueEvent::Final`] 与 [`CueEvent::Translated`]：
/// `Partial` 是视觉占位、`Quality` 是质量上报，都不改变轨道内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueCollector {
    item_id: ItemId,
    origin_lang: Option<String>,
    target_lang: Option<String>,
    cues: Vec<SubtitleCue>,
    last_failed: Option<String>,
    saved: bool,
}

impl CueCollector {
    pub fn new(item_id: ItemId, origin_lang: Option<String>, target_lang: Option<String>) -> Self {
        Self {
            item_id,
            origin_lang,
            target_lang,
            cues: Vec::new(),
            last_failed: None,
            saved: false,
        }
    }

    /// 收一条引擎事件；返回是否产生了**进入轨道**的新内容。
    pub fn absorb(&mut self, event: &CueEvent) -> bool {
        match event {
            CueEvent::Final { cue } => {
                let mut cue = cue.clone();
                if cue.lang.is_none() {
                    cue.lang = self.origin_lang.clone();
                }
                self.cues.push(cue);
                true
            }
            CueEvent::Translated { cue } => {
                let mut cue = cue.clone();
                if cue.lang.is_none() {
                    cue.lang = self.target_lang.clone();
                }
                self.cues.push(cue);
                true
            }
            CueEvent::Partial { .. } | CueEvent::Quality { .. } => false,
            CueEvent::Failed { reason } => {
                self.last_failed = Some(reason.clone());
                false
            }
        }
    }

    /// 丢弃全部旧句（seek 重置句界 / 重新开始时调用，PRD「seek 丢弃旧句」）。
    pub fn reset(&mut self) {
        self.cues.clear();
    }

    /// 是否有尚未保存的 `Final`（关闭会话时需默认落库为轨道并提示）。
    pub fn has_unsaved_finals(&self) -> bool {
        !self.saved && !self.cues.is_empty()
    }

    /// 是否已保存。
    pub fn is_saved(&self) -> bool {
        self.saved
    }

    /// 标记已保存。
    pub fn mark_saved(&mut self) {
        self.saved = true;
    }

    /// 最近一次失败原因。
    pub fn last_failed(&self) -> Option<&str> {
        self.last_failed.as_deref()
    }

    /// 已收集的句子。
    pub fn cues(&self) -> &[SubtitleCue] {
        &self.cues
    }

    /// 归一为一条 `kind=Generated` 的轨道。
    ///
    /// `id` 由调用方（L1/L4）决定；`engine` 记入 `source_engine`，供复用与追溯。
    /// 轨道 `lang` 取目标语种（有译文时表示呈现语言），无译文时取原声语种。
    pub fn into_track(&self, id: impl Into<String>, engine: &str) -> SubtitleTrack {
        let has_translation = self.target_lang.is_some()
            && self
                .cues
                .iter()
                .any(|c| c.lang.as_deref() == self.target_lang.as_deref());
        let lang = if has_translation {
            self.target_lang.clone()
        } else {
            self.origin_lang.clone()
        };
        SubtitleTrack {
            id: id.into(),
            item_id: self.item_id.clone(),
            kind: SubtitleKind::Generated,
            lang,
            source_engine: Some(engine.to_owned()),
            locked: false,
            cues: self.cues.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 外挂字幕文本解析（PRD FR-05 失败出口「加载外挂字幕」）
// ---------------------------------------------------------------------------

/// 解析 SRT / WebVTT / ASS / SSA 文本为字幕句。**纯函数，无 IO**（文件读取在 L3/L4）。
///
/// SRT / WebVTT：宽松解析，按空行分块，块内找到时间行，其后所有非空行拼成该句正文。
/// 时间同时接受 `HH:MM:SS,mmm` 与 `HH:MM:SS.mmm`；`WEBVTT` 头、序号行与 `-->` 之后的
/// cue 设置都被自然跳过。ASS / SSA 按 `[Events]` 的 `Format:` 字段顺序读取
/// `Start` / `End` / `Text`，去掉 `{\...}` 覆盖标签并把 `\N` 还原为换行。
/// 无法解析的块整块忽略。
pub fn parse_subtitle_text(text: &str) -> Vec<SubtitleCue> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if looks_like_ass(&normalized) {
        let cues = parse_ass_text(&normalized);
        if !cues.is_empty() {
            return cues;
        }
    }
    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some((time_idx, start_ms, end_ms)) = lines
            .iter()
            .enumerate()
            .find_map(|(i, line)| parse_time_range(line.trim()).map(|(s, e)| (i, s, e)))
        else {
            continue;
        };
        let body: Vec<String> = lines[time_idx + 1..]
            .iter()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();
        if body.is_empty() {
            continue;
        }
        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text: body.join("\n"),
            lang: None,
        });
    }
    cues
}

/// 解析 ASS / SSA 的 `[Events]` 段。
///
/// 字段顺序以该段的 `Format:` 行为准（`Text` 一般是最后一列，可能含逗号）；
/// 没有 `Format:` 时按常见的 10 列布局兜底。容忍以下真实文件差异：
/// - 段名 / 指令名大小写不一致、`Dialogue` 与冒号间有空格；
/// - 文件开头带 UTF-8 BOM；
/// - 末尾空字段被省略（此时把最后一个字段当正文）；
/// - `Comment:` 等非对话行直接忽略。
fn parse_ass_text(text: &str) -> Vec<SubtitleCue> {
    let mut in_events = false;
    let mut field_count = 10;
    let mut start_idx = 1;
    let mut end_idx = 2;
    let mut text_idx = 9;
    let mut cues = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.starts_with('[') {
            in_events = trimmed.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = strip_prefix_ci(trimmed, "Format") {
            let Some(rest) = rest.trim_start().strip_prefix(':') else {
                continue;
            };
            let fields: Vec<String> = rest
                .split(',')
                .map(|f| f.trim().to_ascii_lowercase())
                .collect();
            if fields.len() >= 3 {
                // 只有三列齐全时才采用该 Format，避免把样式段的坏行当成事件格式
                let start = fields.iter().position(|f| f == "start");
                let end = fields.iter().position(|f| f == "end");
                let text = fields.iter().position(|f| f == "text");
                if let (Some(start), Some(end), Some(text)) = (start, end, text) {
                    field_count = fields.len();
                    start_idx = start;
                    end_idx = end;
                    text_idx = text;
                }
            }
            continue;
        }
        let Some(rest) = strip_prefix_ci(trimmed, "Dialogue") else {
            continue;
        };
        let Some(body) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        // Text 列通常是最后一列：限制分割段数，让正文里的逗号保留下来
        let parts: Vec<&str> = body.splitn(field_count, ',').collect();
        let part_count = parts.len();
        let text_index = if text_idx < part_count {
            text_idx
        } else if text_idx + 1 == field_count && part_count > 0 {
            // 末尾空字段被省略：最后一个字段就是正文
            part_count - 1
        } else {
            continue;
        };
        if start_idx >= part_count || end_idx >= part_count {
            continue;
        }
        if text_index <= start_idx || text_index <= end_idx {
            continue;
        }
        let (Some(start_ms), Some(end_ms)) = (parse_time(parts[start_idx]), parse_time(parts[end_idx])) else {
            continue;
        };
        let body_text = clean_ass_text(parts[text_index].trim());
        if body_text.is_empty() {
            continue;
        }
        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text: body_text,
            lang: None,
        });
    }
    cues
}

/// 忽略大小写匹配前缀，返回其后的文本（按字节安全，不切坏多字节字符）。
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

/// 判断文本是否按 ASS / SSA 解析（`[Events]` 段或 `Dialogue:` 行，不区分大小写）。
fn looks_like_ass(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().trim_start_matches('\u{feff}');
        strip_prefix_ci(line, "[Events]").is_some() || strip_prefix_ci(line, "Dialogue").is_some()
    })
}

/// 去掉 ASS 覆盖标签（`{\an8}` 等）并还原 `\N` / `\n` / `\h`。
fn clean_ass_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '\\' if depth == 0 => match chars.next() {
                Some('N') | Some('n') => out.push('\n'),
                Some('h') => out.push(' '),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}


fn parse_time_range(line: &str) -> Option<(i64, i64)> {
    let (left, right) = line.split_once("-->")?;
    Some((parse_time(left.trim())?, parse_time(right.trim())?))
}

fn parse_time(raw: &str) -> Option<i64> {
    // WebVTT 允许在时间后跟 cue 设置（如 `00:00:01.000 align:start`），只取第一个 token
    let token = raw.split_whitespace().next()?;
    let normalized = token.replace(',', ".");
    let parts: Vec<&str> = normalized.split(':').collect();
    let (h, m, s) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        2 => ("0", parts[0], parts[1]),
        _ => return None,
    };
    let hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    let (secs_text, frac_text) = match s.split_once('.') {
        Some((secs, frac)) => (secs, Some(frac)),
        None => (s, None),
    };
    let secs: i64 = secs_text.parse().ok()?;
    let millis: i64 = match frac_text {
        // 毫秒右补零到三位（`5` → `500`），超过三位截断
        Some(frac) => {
            let head: String = frac.chars().take(3).collect();
            format!("{head:0<3}").parse().ok()?
        }
        None => 0,
    };
    Some(((hours * 60 + minutes) * 60 + secs) * 1000 + millis)
}

// ---------------------------------------------------------------------------
// 单元测试（L2 领域单测）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QualityLevel;

    fn cue(start: i64, end: i64, text: &str) -> SubtitleCue {
        SubtitleCue {
            start_ms: start,
            end_ms: end,
            text: text.to_owned(),
            lang: None,
        }
    }

    #[test]
    fn state_machine_follows_documented_lifecycle() {
        assert_eq!(
            RealtimeState::Idle.on(RealtimeEvent::Start),
            Some(RealtimeState::Initializing)
        );
        assert_eq!(
            RealtimeState::Initializing.on(RealtimeEvent::Ready),
            Some(RealtimeState::Running)
        );
        assert_eq!(
            RealtimeState::Running.on(RealtimeEvent::Pause),
            Some(RealtimeState::Paused)
        );
        assert_eq!(
            RealtimeState::Paused.on(RealtimeEvent::Resume),
            Some(RealtimeState::Running)
        );
        assert_eq!(
            RealtimeState::Running.on(RealtimeEvent::Saved),
            Some(RealtimeState::Saved)
        );
        assert_eq!(
            RealtimeState::Running.on(RealtimeEvent::Fail),
            Some(RealtimeState::Failed)
        );
        // 失败后原地重启
        assert_eq!(
            RealtimeState::Failed.on(RealtimeEvent::Restart),
            Some(RealtimeState::Initializing)
        );
        // 任何活动态都可停止回到空闲
        assert_eq!(
            RealtimeState::Paused.on(RealtimeEvent::Stop),
            Some(RealtimeState::Idle)
        );
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert_eq!(RealtimeState::Idle.on(RealtimeEvent::Pause), None);
        assert_eq!(RealtimeState::Idle.on(RealtimeEvent::Ready), None);
        assert_eq!(RealtimeState::Saved.on(RealtimeEvent::Resume), None);
        assert_eq!(RealtimeState::Failed.on(RealtimeEvent::Ready), None);
    }

    #[test]
    fn active_states_hold_the_asr_child() {
        assert!(!RealtimeState::Idle.is_active());
        assert!(RealtimeState::Initializing.is_active());
        assert!(RealtimeState::Running.is_active());
        assert!(RealtimeState::Paused.is_active());
        assert!(!RealtimeState::Failed.is_active());
        assert!(!RealtimeState::Saved.is_active());
    }

    #[test]
    fn session_cfg_prefers_manual_origin_and_defaults_target_to_chinese() {
        let cfg = session_cfg(None, None, CueDisplay::Bilingual);
        assert_eq!(cfg.origin_lang, None, "未指定则交给引擎自动判定");
        assert_eq!(cfg.target_lang.as_deref(), Some(DEFAULT_TARGET_LANG));
        assert!(cfg.translate, "双语且目标语种未知 → 翻译开启");

        let manual = session_cfg(Some("ja"), None, CueDisplay::Bilingual);
        assert_eq!(manual.origin_lang.as_deref(), Some("ja"), "手动指定优先");
        assert!(manual.translate, "日语 → 简体中文需要翻译");
    }

    #[test]
    fn session_cfg_skips_translation_for_same_language_or_origin_only() {
        // 目标语言 = 原声语种（只听写不翻译）
        let same = session_cfg(Some("zh"), Some("zh"), CueDisplay::Bilingual);
        assert!(!same.translate);

        // 「仅原文」不需要翻译
        let origin_only = session_cfg(Some("en"), None, CueDisplay::OriginOnly);
        assert!(!origin_only.translate);

        // 「仅译文」仍需要翻译
        let translation_only = session_cfg(Some("en"), None, CueDisplay::TranslationOnly);
        assert!(translation_only.translate);
    }

    #[test]
    fn degrade_actions_map_from_watermark() {
        assert_eq!(
            degrade_for(Watermark::Ok, CueDisplay::Bilingual),
            DegradeAction::None
        );
        assert_eq!(
            degrade_for(Watermark::High, CueDisplay::Bilingual),
            DegradeAction::OriginOnly
        );
        assert_eq!(
            degrade_for(Watermark::High, CueDisplay::OriginOnly),
            DegradeAction::None,
            "已经是仅原文时无需再降级"
        );
        assert_eq!(
            degrade_for(Watermark::Critical, CueDisplay::Bilingual),
            DegradeAction::Pause
        );
        assert_eq!(
            degrade_for(Watermark::Critical, CueDisplay::OriginOnly),
            DegradeAction::Pause
        );
    }

    #[test]
    fn entry_is_offered_only_without_any_subtitle_track() {
        assert!(should_offer_entry(false, false));
        assert!(!should_offer_entry(true, false), "有内嵌字幕不出现入口");
        assert!(!should_offer_entry(false, true), "有外挂字幕不出现入口");
        assert!(!should_offer_entry(true, true));
    }

    #[test]
    fn failure_exits_cover_prd_three_options() {
        assert_eq!(FailureExit::ALL.len(), 3);
        for exit in FailureExit::ALL {
            assert_eq!(FailureExit::from_tag(exit.tag()), Some(exit));
            assert!(!exit.label().is_empty());
        }
        assert_eq!(FailureExit::from_tag("未知"), None);
    }

    #[test]
    fn cue_collector_ignores_partials_and_quality() {
        let mut collector = CueCollector::new(ItemId::from("i1"), Some("en".into()), Some("zh".into()));
        assert!(!collector.absorb(&CueEvent::Partial {
            text: "hel".into(),
            conf_milli: 500,
        }));
        assert!(!collector.absorb(&CueEvent::Quality {
            level: QualityLevel::Degraded,
        }));

        assert!(collector.absorb(&CueEvent::Final {
            cue: cue(0, 1000, "hello"),
        }));
        assert!(collector.absorb(&CueEvent::Translated {
            cue: cue(0, 1000, "你好"),
        }));

        assert_eq!(collector.cues().len(), 2, "只有 Final / Translated 进入轨道");
        assert_eq!(collector.cues()[0].lang.as_deref(), Some("en"), "原文句标记原声语种");
        assert_eq!(collector.cues()[1].lang.as_deref(), Some("zh"), "译文句标记目标语种");
        assert!(collector.has_unsaved_finals());
    }

    #[test]
    fn cue_collector_reset_drops_old_sentences_on_seek() {
        let mut collector = CueCollector::new(ItemId::from("i1"), None, Some("zh".into()));
        collector.absorb(&CueEvent::Final {
            cue: cue(0, 1000, "旧句"),
        });
        collector.reset();
        assert!(collector.cues().is_empty(), "seek 后必须丢弃旧句");
        assert!(!collector.has_unsaved_finals());
    }

    #[test]
    fn cue_collector_normalizes_into_generated_track() {
        let mut collector = CueCollector::new(ItemId::from("i1"), Some("en".into()), Some("zh".into()));
        collector.absorb(&CueEvent::Final {
            cue: cue(0, 1000, "hello"),
        });
        let track = collector.into_track("t1", "local-whisper");
        assert_eq!(track.id, "t1");
        assert_eq!(track.item_id, ItemId::from("i1"));
        assert_eq!(track.kind, SubtitleKind::Generated);
        assert_eq!(track.source_engine.as_deref(), Some("local-whisper"));
        assert!(!track.locked);
        assert_eq!(track.cues.len(), 1);

        // 保存后不再算「未保存」
        collector.mark_saved();
        assert!(!collector.has_unsaved_finals());
    }

    #[test]
    fn srt_text_parses_into_cues() {
        let text = "1\n00:00:01,000 --> 00:00:03,500\n你好\n世界\n\n2\n00:00:04,000 --> 00:00:05,000\n第二句\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 3500);
        assert_eq!(cues[0].text, "你好\n世界", "同句多行合并");
        assert_eq!(cues[1].text, "第二句");
        assert_eq!(cues[1].start_ms, 4000);
    }

    #[test]
    fn webvtt_headers_and_settings_are_skipped() {
        let text = "WEBVTT\n\n00:00:01.500 --> 00:00:03.000 align:start\nHi\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 1500, "点号分隔同样解析");
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn malformed_blocks_are_ignored() {
        assert!(parse_subtitle_text("这是一段没有任何时间行的文本").is_empty());
        assert!(parse_subtitle_text("1\n00:00:01,000 --> 00:00:02,000\n").is_empty(), "无正文的块忽略");
    }

    #[test]
    fn ass_dialogue_lines_parse_into_cues() {
        let text = "[Script Info]\nTitle: t\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,楷体\n\n[Events]\nFormat: Layer, Start, End, Style, Actor, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.23,0:00:05.66,Default,,0,0,0,,{\\an8}第一行\\N第二行\nDialogue: 0,0:01:07.93,0:01:08.99,Default,,0,0,0,,逗号,保留\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 1230, "ASS 时间按百分秒");
        assert_eq!(cues[0].end_ms, 5660);
        assert_eq!(cues[0].text, "第一行\n第二行", "覆盖标签去除、\\N 换行");
        assert_eq!(cues[1].text, "逗号,保留", "正文中的逗号保留");
    }

    #[test]
    fn ass_tolerates_bom_case_and_missing_tail_fields() {
        let text = "\u{feff}[Script Info]\nTitle: t\n\n[STYLES]\nFormat: Name, Fontname\nStyle: Default,a\n\n[events]\nformat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nComment: 0,0:00:00.00,0:00:00.00,Default,,0,0,0,,注释行\nDialogue : 0,0:00:02.00,0:00:04.00,Default,,0,0,0,末字段省略\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 1, "Comment 行忽略、小写段名与 BOM 兼容");
        assert_eq!(cues[0].start_ms, 2000);
        assert_eq!(cues[0].end_ms, 4000);
        assert_eq!(cues[0].text, "末字段省略");
    }

    #[test]
    fn ass_text_cleans_inline_overrides_and_hard_space() {
        let text = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,{\\pos(320,50)\\c&H00FFFF&}前\\h中{\\i1}后{\\i0}\\n尾\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "前 中后\n尾", "行内标签去除、\\h 变空格、\\n 换行");
    }

    #[test]
    fn ass_format_order_from_header_is_respected() {
        let text = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:10.50,0:00:12.00,只有三列\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 10_500);
        assert_eq!(cues[0].end_ms, 12_000);
        assert_eq!(cues[0].text, "只有三列");
    }

    #[test]
    fn ass_drawing_and_empty_cues_are_dropped() {
        let text = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\an8}\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,有效\n";
        let cues = parse_subtitle_text(text);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "有效");
    }
}
