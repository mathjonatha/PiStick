
use std::sync::Arc;
use std::time::Duration;

use ps_codec::Rect;
use ps_proto::{CursorShape, Message};

#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    Created { win: u32, width: u32, height: u32, title: String, app_id: String },
    Resized { win: u32, width: u32, height: u32 },
    Title { win: u32, title: String },
    Destroyed { win: u32 },
    Damage { win: u32, rects: Vec<Rect> },
    PopupCreated { win: u32, parent: u32, x: i32, y: i32, width: u32, height: u32, grab: bool },
    PopupMoved { win: u32, x: i32, y: i32, width: u32, height: u32 },
    Parent { win: u32, parent: u32 },
    Limits { win: u32, min_w: u32, min_h: u32, max_w: u32, max_h: u32 },
    Cursor { win: u32, shape: CursorShape },
    Clipboard { mime: String, data: Vec<u8> },
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub trait WindowSource: Send {
    fn set_client_caps(&mut self, _caps: u32) {}

    fn start(&mut self, waker: Waker) -> Vec<SourceEvent>;

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent>;

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration>;

    fn with_pixels(&mut self, win: u32, f: &mut dyn FnMut(u32, u32, &[u8]));

    fn input(&mut self, msg: &Message);

    fn frame_sent(&mut self, win: u32);

    fn stop(&mut self) {}
}
