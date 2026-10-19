//! P-03 引擎与识别子进程之间的**线协议**（std 手写，不引入任何序列化依赖）。
//!
//! 两个方向各用一套编码，刻意保持极简：
//! - 引擎 → 子进程：stdin 上的**二进制帧流**（控制信号 + PCM），见 [`Cmd`]。
//! - 子进程 → 引擎：stdout 上的**文本记录行**（Tab 分隔、字段转义），与
//!   `crates/infra` 的字幕句编码风格一致，保证往返无损。
//!
//! stderr 留给子进程写日志（引擎把它设为 null），因此 stdout 上只允许出现记录行。

use std::io::Read;

use cinema_domain::model::{CueEvent, QualityLevel, SubtitleCue};

/// 单个帧载荷的尺寸上限（防御异常输入导致的巨额分配）。
const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// 引擎下发给子进程的指令。
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// 一帧 16k 单声道 PCM 样本。
    Audio(Vec<i16>),
    /// 挂起：保留末句，不再输出。
    Pause,
    /// 继续。
    Resume,
    /// 进度跳转：清空缓冲、重置句界、丢弃旧句。
    Seek(i64),
    /// 释放并退出。
    Stop,
}

fn invalid(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// 音频帧：`b'A'` + u32 小端长度（字节数） + i16 小端样本字节。
pub fn audio_frame(pcm: &[i16]) -> Vec<u8> {
    let len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(5 + len as usize);
    out.push(b'A');
    out.extend_from_slice(&len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// 无载荷控制帧（pause=`P` / resume=`R` / stop=`X`）。
pub fn control_frame(tag: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(tag);
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// seek 帧：`b'S'` + u32 小端长度(8) + i64 小端毫秒。
pub fn seek_frame(pos_ms: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.push(b'S');
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&pos_ms.to_le_bytes());
    out
}

/// 从流中读一帧。读到干净 EOF 返回 `Ok(None)`；未知 tag 或长度异常返回 `InvalidData`。
pub fn read_cmd<R: Read>(r: &mut R) -> std::io::Result<Option<Cmd>> {
    // 首字节若直接 EOF，说明对端正常关闭
    let mut tag = [0u8; 1];
    loop {
        match r.read(&mut tag) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(invalid(format!("帧长度异常：{len}")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;

    match tag[0] {
        b'A' => {
            if len % 2 != 0 {
                return Err(invalid("音频帧载荷长度必须为偶数"));
            }
            let mut pcm = Vec::with_capacity(len / 2);
            for pair in payload.chunks_exact(2) {
                pcm.push(i16::from_le_bytes([pair[0], pair[1]]));
            }
            Ok(Some(Cmd::Audio(pcm)))
        }
        b'P' => {
            if len != 0 {
                return Err(invalid("pause 帧不得携带载荷"));
            }
            Ok(Some(Cmd::Pause))
        }
        b'R' => {
            if len != 0 {
                return Err(invalid("resume 帧不得携带载荷"));
            }
            Ok(Some(Cmd::Resume))
        }
        b'X' => {
            if len != 0 {
                return Err(invalid("stop 帧不得携带载荷"));
            }
            Ok(Some(Cmd::Stop))
        }
        b'S' => {
            if len != 8 {
                return Err(invalid("seek 帧载荷必须为 8 字节"));
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&payload);
            Ok(Some(Cmd::Seek(i64::from_le_bytes(raw))))
        }
        other => Err(invalid(format!("未知帧标签 0x{other:02x}"))),
    }
}

// ---------------------------------------------------------------------------
// 记录行编解码（子进程 → 引擎）
// ---------------------------------------------------------------------------

/// 字段转义：`\`→`\\`、Tab→`\t`、CR→`\r`、LF→`\n`。与 L4 的 cue 编码风格一致。
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// 与 [`escape`] 互逆。
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn quality_to_str(level: QualityLevel) -> &'static str {
    match level {
        QualityLevel::Ok => "ok",
        QualityLevel::Degraded => "degraded",
        QualityLevel::Critical => "critical",
    }
}

fn quality_from_str(raw: &str) -> Option<QualityLevel> {
    match raw {
        "ok" => Some(QualityLevel::Ok),
        "degraded" => Some(QualityLevel::Degraded),
        "critical" => Some(QualityLevel::Critical),
        _ => None,
    }
}

/// 事件 → 单行记录（不含换行）。
pub fn encode_record(ev: &CueEvent) -> String {
    match ev {
        CueEvent::Partial { text, conf_milli } => {
            format!("partial\t{}\t{}", escape(text), conf_milli)
        }
        CueEvent::Final { cue } => format!(
            "final\t{}\t{}\t{}\t{}",
            cue.start_ms,
            cue.end_ms,
            cue.lang.as_deref().unwrap_or(""),
            escape(&cue.text)
        ),
        CueEvent::Translated { cue } => format!(
            "translated\t{}\t{}\t{}\t{}",
            cue.start_ms,
            cue.end_ms,
            cue.lang.as_deref().unwrap_or(""),
            escape(&cue.text)
        ),
        CueEvent::Quality { level } => format!("quality\t{}", quality_to_str(*level)),
        CueEvent::Failed { reason } => format!("failed\t{}", escape(reason)),
    }
}

fn cue_from_parts(start_ms: i64, end_ms: i64, lang: &str, text: String) -> SubtitleCue {
    SubtitleCue {
        start_ms,
        end_ms,
        text,
        lang: (!lang.is_empty()).then(|| lang.to_owned()),
    }
}

/// 单行记录 → 事件。无法解析返回 `None`（不猜测）。
pub fn decode_record(line: &str) -> Option<CueEvent> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(5, '\t');
    let kind = parts.next()?;
    match kind {
        "partial" => {
            let text = unescape(parts.next()?);
            let conf_milli = parts.next()?.trim().parse().ok()?;
            Some(CueEvent::Partial { text, conf_milli })
        }
        "final" => {
            let start_ms = parts.next()?.trim().parse().ok()?;
            let end_ms = parts.next()?.trim().parse().ok()?;
            let lang = parts.next()?;
            let text = unescape(parts.next().unwrap_or(""));
            Some(CueEvent::Final {
                cue: cue_from_parts(start_ms, end_ms, lang, text),
            })
        }
        "translated" => {
            let start_ms = parts.next()?.trim().parse().ok()?;
            let end_ms = parts.next()?.trim().parse().ok()?;
            let lang = parts.next()?;
            let text = unescape(parts.next().unwrap_or(""));
            Some(CueEvent::Translated {
                cue: cue_from_parts(start_ms, end_ms, lang, text),
            })
        }
        "quality" => {
            let level = quality_from_str(parts.next()?.trim())?;
            Some(CueEvent::Quality { level })
        }
        "failed" => Some(CueEvent::Failed {
            reason: unescape(parts.next().unwrap_or("")),
        }),
        _ => None,
    }
}
