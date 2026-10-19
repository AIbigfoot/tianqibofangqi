//! 本地识别子进程 `cinema-asr`：stdin 收二进制帧流，stdout 吐记录行。
//!
//! 只依赖 std 与同包的 [`cinema_adapters::subtitle_proto`]，离线可用。
//! **stdout 上只允许出现记录行**（调试一律走 stderr），且逐行 flush，否则引擎读不到。

use std::io::{self, Write};

use cinema_adapters::subtitle_proto::{encode_record, read_cmd, Cmd};
use cinema_domain::model::{CueEvent, QualityLevel, SubtitleCue};

/// 脚本化后端每句固定时长（毫秒）。
const SEGMENT_MS: i64 = 1000;
/// 能量 VAD 的分析窗：100ms（16k 单声道）。
const VAD_WINDOW: usize = 1600;
/// 判定为「有声」的 RMS 下限。
const VAD_RMS: f64 = 300.0;
/// 连续静音窗数达到该值即切句（约 300ms）。
const VAD_SILENT_WINDOWS: usize = 3;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut child = Child::new(&args);
    if let Err(err) = run(&mut child) {
        // 仅供调试；引擎已把 stderr 设为 null
        eprintln!("cinema-asr 退出：{err}");
    }
}

fn run(child: &mut Child) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        match read_cmd(&mut reader)? {
            None => break, // stdin EOF：正常退出
            Some(Cmd::Stop) => break,
            Some(Cmd::Pause) => child.paused = true,
            Some(Cmd::Resume) => child.paused = false,
            Some(Cmd::Seek(ms)) => child.seek(ms),
            Some(Cmd::Audio(pcm)) => child.feed(&pcm),
        }
    }
    Ok(())
}

/// 取 `--key` 后面的值（简单遍历，不引入参数解析依赖）。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let mut i = 0;
    while i + 1 < args.len() {
        if args[i] == key {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

/// 解析 `原文=>译文|原文2=>译文2`；无 `=>` 时视为只有原文。
fn parse_script(raw: &str) -> Vec<(String, String)> {
    raw.split('|')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }
            match segment.split_once("=>") {
                Some((origin, translation)) => {
                    Some((origin.trim().to_owned(), translation.trim().to_owned()))
                }
                None => Some((segment.to_owned(), String::new())),
            }
        })
        .collect()
}

/// 未配置脚本时使用的内置默认对。
fn default_script() -> Vec<(String, String)> {
    vec![
        ("Hello world".to_owned(), "你好，世界".to_owned()),
        ("How are you today".to_owned(), "你今天好吗".to_owned()),
        ("See you next time".to_owned(), "下次见".to_owned()),
    ]
}

fn emit(ev: &CueEvent) {
    let line = encode_record(ev);
    println!("{line}");
    let _ = io::stdout().flush();
}

struct Child {
    backend: String,
    translate: bool,
    origin_lang: Option<String>,
    target_lang: Option<String>,
    paused: bool,
    /// 已上报致命失败（缺模型等），此后不再输出正常结果。
    failed: bool,
    /// 脚本化后端的分句表与游标。
    script: Vec<(String, String)>,
    script_idx: usize,
    /// 时间轴累计（毫秒）。
    clock_ms: i64,
    /// 能量 VAD 累积缓冲。
    buffer: Vec<i16>,
    /// 前一窗是否有声。
    voiced: bool,
    /// 连续静音窗计数。
    silent_windows: usize,
    /// 当前句起始时刻（毫秒）。
    sentence_start_ms: i64,
    /// 当前句累计时长（毫秒）。
    sentence_ms: i64,
}

impl Child {
    fn new(args: &[String]) -> Self {
        let backend = arg_value(args, "--backend").unwrap_or_else(|| "whisper".to_owned());
        let translate = arg_value(args, "--translate").as_deref() == Some("yes");
        let origin_lang = arg_value(args, "--origin").filter(|v| !v.is_empty() && v != "auto");
        let target_lang = arg_value(args, "--target").filter(|v| !v.is_empty());
        let script = arg_value(args, "--script")
            .map(|raw| parse_script(&raw))
            .filter(|list| !list.is_empty())
            .unwrap_or_else(default_script);

        let mut child = Self {
            backend,
            translate,
            origin_lang,
            target_lang,
            paused: false,
            failed: false,
            script,
            script_idx: 0,
            clock_ms: 0,
            buffer: Vec::new(),
            voiced: false,
            silent_windows: 0,
            sentence_start_ms: 0,
            sentence_ms: 0,
        };

        // 真实后端需要模型；缺失时立即上报失败（不再输出正常结果）。
        if child.backend == "whisper" {
            match arg_value(args, "--model") {
                None => child.fail("未配置本地识别模型"),
                Some(model) if !std::path::Path::new(&model).exists() => {
                    child.fail(&format!("本地识别模型不可用：{model}"));
                }
                Some(_) => {
                    // 说明：真实模型推理尚未接入，下面走占位路径（能量 VAD 切句 +
                    // 占位文本 + degraded 质量），仅用于打通「音频 → 成句」的数据流。
                    emit(&CueEvent::Quality {
                        level: QualityLevel::Degraded,
                    });
                }
            }
        }
        child
    }

    fn fail(&mut self, reason: &str) {
        self.failed = true;
        emit(&CueEvent::Failed {
            reason: reason.to_owned(),
        });
    }

    fn feed(&mut self, pcm: &[i16]) {
        if self.paused || self.failed {
            return; // 挂起时保留末句；失败后不再输出
        }
        if self.backend == "scripted" {
            self.emit_scripted();
        } else {
            self.feed_whisper(pcm);
        }
    }

    fn seek(&mut self, position_ms: i64) {
        self.buffer.clear();
        self.voiced = false;
        self.silent_windows = 0;
        self.sentence_ms = 0;
        self.clock_ms = position_ms;
    }

    fn cue(&self, start_ms: i64, end_ms: i64, text: String, lang: Option<String>) -> SubtitleCue {
        SubtitleCue {
            start_ms,
            end_ms,
            text,
            lang,
        }
    }

    /// 脚本化后端：每收一帧就推进一句，输出 final（+ translated）。
    fn emit_scripted(&mut self) {
        let (origin, translation) = self.script[self.script_idx % self.script.len()].clone();
        self.script_idx += 1;
        let start = self.clock_ms;
        let end = start + SEGMENT_MS;
        self.clock_ms = end;

        emit(&CueEvent::Final {
            cue: self.cue(start, end, origin, self.origin_lang.clone()),
        });
        if self.translate {
            emit(&CueEvent::Translated {
                cue: self.cue(start, end, translation, self.target_lang.clone()),
            });
        }
    }

    /// 真实后端的最小占位路径：能量 VAD 切句（见 [`Child::new`] 的占位说明）。
    fn feed_whisper(&mut self, pcm: &[i16]) {
        self.buffer.extend_from_slice(pcm);
        while self.buffer.len() >= VAD_WINDOW {
            let window: Vec<i16> = self.buffer.drain(..VAD_WINDOW).collect();
            let rms = (window
                .iter()
                .map(|s| (*s as f64) * (*s as f64))
                .sum::<f64>()
                / window.len() as f64)
                .sqrt();
            let voiced = rms >= VAD_RMS;

            if voiced {
                if self.sentence_ms == 0 {
                    self.sentence_start_ms = self.clock_ms;
                }
                self.sentence_ms += (VAD_WINDOW as i64) / 16; // 1600 样本 @16k = 100ms
                self.silent_windows = 0;
            } else if self.sentence_ms > 0 {
                self.silent_windows += 1;
                if self.silent_windows >= VAD_SILENT_WINDOWS {
                    self.close_sentence();
                }
            }
            self.clock_ms += (VAD_WINDOW as i64) / 16;
        }
    }

    /// 句尾输出成句（占位文本），并给出译文。
    fn close_sentence(&mut self) {
        let start = self.sentence_start_ms;
        let end = start + self.sentence_ms;
        let text = "[识别结果]".to_owned();
        emit(&CueEvent::Final {
            cue: self.cue(start, end, text.clone(), self.origin_lang.clone()),
        });
        if self.translate {
            emit(&CueEvent::Translated {
                cue: self.cue(start, end, text, self.target_lang.clone()),
            });
        }
        self.sentence_ms = 0;
        self.silent_windows = 0;
    }
}
