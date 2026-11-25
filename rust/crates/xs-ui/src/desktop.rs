//! 桌面宿主与渲染子进程的有限消息，以及双 Ctrl 的手势识别。
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read, Write};

#[cfg(windows)]
#[path = "desktop_windows.rs"]
pub mod windows;

pub const MAX_QUERY_BYTES: usize = 4096;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    #[default]
    Main,
    Quick,
}

impl View {
    pub fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Quick => 1,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HostMessage {
    Boot { view: View, queries: [String; 2] },
    Show { view: View, query: Option<String> },
    FileDialog(isize),
    Hide,
    ScanStarted,
    ScanProgress(xs_platform::ScanProgress),
    ScanFinished(Result<crate::ScanSummary, String>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RendererMessage {
    Ready,
    Failure(String),
    View(View),
    Query { view: View, query: String },
    Hidden(usize),
    Closing,
    StartScan { root: Option<std::path::PathBuf> },
    CancelScan,
}

/// 每帧必须以换行结束。在分配缓冲区之前限制从管道读取的长度。
pub fn receive<T: for<'de> Deserialize<'de>>(input: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut frame = Vec::new();
    if input
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)?
        == 0
    {
        return Ok(None);
    }
    if frame.len() > MAX_FRAME_BYTES || frame.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "桌面消息超出限制或未结束",
        ));
    }
    serde_json::from_slice(&frame)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn send<T: Serialize>(output: &mut impl Write, value: &T) -> io::Result<()> {
    let mut frame = serde_json::to_vec(value).map_err(io::Error::other)?;
    if frame.len() >= MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "桌面消息超出限制",
        ));
    }
    frame.push(b'\n');
    output.write_all(&frame)?;
    output.flush()
}

/// 保留原始 UTF-8 字符边界，并限制启动参数和跨进程查询长度。
pub fn limit_query(value: &mut String) {
    limit_text(value, MAX_QUERY_BYTES);
}

pub fn limit_text(value: &mut String, max: usize) {
    if value.len() > max {
        let mut end = max;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy)]
pub enum Key {
    LeftCtrl,
    RightCtrl,
    Other,
}

#[cfg(any(windows, test))]
#[derive(Default)]
pub struct DoubleCtrl {
    ctrl: u8,
    chord: bool,
    down: u32,
    last_tap: Option<u32>,
}

#[cfg(any(windows, test))]
impl DoubleCtrl {
    /// 回调只追踪 Ctrl 手势的状态。其它按键立即使当前手势失效。
    pub fn event(&mut self, key: Key, pressed: bool, timestamp: u32) -> bool {
        const MAX_HOLD_MS: u32 = 250;
        const MAX_GAP_MS: u32 = 400;
        let mask = match key {
            Key::LeftCtrl => 1,
            Key::RightCtrl => 2,
            Key::Other => {
                self.last_tap = None;
                self.chord |= self.ctrl != 0;
                return false;
            }
        };
        if pressed {
            if self.ctrl & mask == 0 {
                if self.ctrl == 0 {
                    self.down = timestamp;
                    self.chord = false;
                } else {
                    self.chord = true;
                    self.last_tap = None;
                }
                self.ctrl |= mask;
            }
            return false;
        }
        if self.ctrl & mask == 0 {
            self.last_tap = None;
            return false;
        }
        self.ctrl &= !mask;
        if self.ctrl != 0 || self.chord || timestamp.wrapping_sub(self.down) > MAX_HOLD_MS {
            self.last_tap = None;
            return false;
        }
        if self
            .last_tap
            .take()
            .is_some_and(|previous| self.down.wrapping_sub(previous) <= MAX_GAP_MS)
        {
            true
        } else {
            self.last_tap = Some(timestamp);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap(state: &mut DoubleCtrl, key: Key, start: u32) -> bool {
        assert!(!state.event(key, true, start));
        state.event(key, false, start.wrapping_add(30))
    }

    #[test]
    fn double_ctrl_accepts_two_taps_across_tick_wrap() {
        let mut state = DoubleCtrl::default();
        assert!(!tap(&mut state, Key::LeftCtrl, u32::MAX - 35));
        assert!(tap(&mut state, Key::RightCtrl, 40));
        assert!(!tap(&mut state, Key::RightCtrl, 120));
        assert!(!tap(&mut state, Key::RightCtrl, 650));
    }

    #[test]
    fn double_ctrl_ignores_chords_long_holds_and_key_repeat() {
        let mut state = DoubleCtrl::default();
        assert!(!tap(&mut state, Key::LeftCtrl, 0));
        assert!(!state.event(Key::LeftCtrl, true, 80));
        assert!(!state.event(Key::Other, true, 90));
        assert!(!state.event(Key::LeftCtrl, false, 110));
        assert!(!tap(&mut state, Key::LeftCtrl, 150));
        assert!(!state.event(Key::LeftCtrl, true, 210));
        assert!(!state.event(Key::LeftCtrl, true, 310));
        assert!(!state.event(Key::LeftCtrl, false, 500));
        assert!(!state.event(Key::LeftCtrl, true, 550));
        assert!(!state.event(Key::RightCtrl, true, 570));
        assert!(!state.event(Key::LeftCtrl, false, 585));
        assert!(!state.event(Key::RightCtrl, false, 600));
        assert!(!tap(&mut state, Key::LeftCtrl, 640));
    }

    #[test]
    fn renderer_protocol_preserves_unicode_and_rejects_bad_frames() {
        let mut text = "文".repeat(MAX_QUERY_BYTES);
        limit_query(&mut text);
        assert!(text.len() <= MAX_QUERY_BYTES);
        let mut wire = Vec::new();
        send(
            &mut wire,
            &RendererMessage::Query {
                view: View::Quick,
                query: text.clone(),
            },
        )
        .unwrap();
        let got = receive::<RendererMessage>(&mut wire.as_slice())
            .unwrap()
            .unwrap();
        assert!(
            matches!(got, RendererMessage::Query { view: View::Quick, query } if query == text)
        );
        assert!(receive::<HostMessage>(&mut &b"\"Hide\""[..]).is_err());
        assert!(
            receive::<RendererMessage>(&mut vec![b'x'; MAX_FRAME_BYTES + 1].as_slice()).is_err()
        );
    }
}
