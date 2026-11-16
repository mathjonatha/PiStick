
use std::fmt;
use std::io::{self, Read, Write};

pub const MAGIC: u32 = 0x4B54_5350;
pub const VERSION: u16 = 1;
pub const MAX_BODY_LEN: u32 = 64 * 1024 * 1024;
pub const HEADER_LEN: usize = 8;

pub const CAP_BLOCK_CODEC: u32 = 1 << 0;
pub const CAP_H264: u32 = 1 << 1;

pub const CAP_POPUPS: u32 = 1 << 2;
pub const CAP_CURSOR: u32 = 1 << 3;
pub const CAP_CLIPBOARD: u32 = 1 << 4;
pub const CAP_AUDIO: u32 = 1 << 5;
pub const CAP_ATOMIC: u32 = 1 << 6;
pub const CAP_MIC: u32 = 1 << 7;
pub const CAP_SESSION: u32 = 1 << 8;
pub const CAPS_MARCO3: u32 = CAP_POPUPS | CAP_CURSOR | CAP_CLIPBOARD;

pub const MAX_CLIPBOARD: usize = 1024 * 1024;
pub const MAX_CURSOR_SIDE: u16 = 128;
pub const MIME_TEXT: &str = "text/plain;charset=utf-8";

pub const MAX_AUDIO_PAYLOAD: usize = 64 * 1024;
pub const AUDIO_STREAM_OUT: u8 = 0;
pub const AUDIO_STREAM_MIC: u8 = 1;
pub const AUDIO_FLAG_DISCONTINUITY: u8 = 1;
pub const AUDIO_FLAG_DROPPED: u8 = 2;

pub const FRAME_FLAG_MORE: u16 = 1;

pub const CODEC_BLOCKS: u8 = 1;
pub const CODEC_H264: u8 = 1;
pub const VIDEO_FLAG_KEY: u8 = 1;

pub const AUTH_PAIRED: u8 = 0;
pub const AUTH_PAIRED_NOW: u8 = 1;
pub const AUTH_PENDING: u8 = 2;
pub const AUTH_REFUSED: u8 = 3;

pub const GOODBYE_CLIENT: u8 = 0;
pub const GOODBYE_REPLACED: u8 = 1;
pub const GOODBYE_SHUTDOWN: u8 = 2;
pub const MAX_GOODBYE_TEXT: usize = 255;

pub const STATUS_UNDERVOLT_NOW: u32 = 1 << 0;
pub const STATUS_UNDERVOLT_SINCE_BOOT: u32 = 1 << 1;
pub const STATUS_THROTTLED_NOW: u32 = 1 << 2;
pub const STATUS_DISPOSABLE: u32 = 1 << 3;
pub const STATUS_TEMP_UNKNOWN: i16 = i16::MIN;

pub mod kind {
    pub const HELLO: u16 = 1;
    pub const WIN_CREATE: u16 = 16;
    pub const WIN_RESIZE: u16 = 17;
    pub const WIN_TITLE: u16 = 18;
    pub const WIN_DESTROY: u16 = 19;
    pub const FRAME: u16 = 20;
    pub const POPUP_CREATE: u16 = 21;
    pub const CURSOR: u16 = 22;
    pub const CLIPBOARD: u16 = 23;
    pub const WIN_PARENT: u16 = 24;
    pub const WIN_LIMITS: u16 = 25;
    pub const POPUP_MOVE: u16 = 26;
    pub const VIDEO_START: u16 = 27;
    pub const VIDEO_FRAME: u16 = 28;
    pub const VIDEO_STOP: u16 = 29;
    pub const PING: u16 = 30;
    pub const PONG: u16 = 31;
    pub const FRAME_ACK: u16 = 32;
    pub const POINTER_MOVE: u16 = 33;
    pub const POINTER_BUTTON: u16 = 34;
    pub const POINTER_AXIS: u16 = 35;
    pub const KEY: u16 = 36;
    pub const RESIZE_REQ: u16 = 37;
    pub const CLOSE_REQ: u16 = 38;
    pub const FOCUS: u16 = 39;
    pub const CLIPBOARD_UP: u16 = 40;
    pub const AUDIO_START: u16 = 41;
    pub const AUDIO_DATA: u16 = 42;
    pub const AUDIO_STOP: u16 = 43;
    pub const AUTH_STATUS: u16 = 44;
    pub const AUDIO_REQUEST: u16 = 45;
    pub const GOODBYE: u16 = 46;
    pub const STATUS: u16 = 47;
}

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    Closed,
    TooLarge(u32),
    Truncated,
    BadUtf8,
    BadMagic(u32),
    BadVersion(u16),
    StringTooLong(usize),
    Unexpected(u16),
    Invalid(&'static str),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Io(e) => write!(f, "erro de E/S: {e}"),
            ProtoError::Closed => write!(f, "conexão fechada pela outra ponta"),
            ProtoError::TooLarge(n) => write!(f, "mensagem grande demais ({n} bytes)"),
            ProtoError::Truncated => write!(f, "mensagem truncada"),
            ProtoError::BadUtf8 => write!(f, "texto não é UTF-8 válido"),
            ProtoError::BadMagic(m) => write!(f, "magic inválido no HELLO: {m:#010x}"),
            ProtoError::BadVersion(v) => write!(f, "versão de protocolo incompatível: {v}"),
            ProtoError::StringTooLong(n) => write!(f, "texto longo demais ({n} bytes)"),
            ProtoError::Unexpected(k) => write!(f, "mensagem inesperada (kind {k}) em vez de HELLO"),
            ProtoError::Invalid(motivo) => write!(f, "valor inválido: {motivo}"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProtoError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> Self {
        ProtoError::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CursorShape {
    Hidden,
    Named(String),
    Image { width: u16, height: u16, hot_x: i16, hot_y: i16, bgra: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello { version: u16, caps: u32, name: String },
    WinCreate { win: u32, width: u32, height: u32, title: String, app_id: String },
    WinResize { win: u32, width: u32, height: u32 },
    WinTitle { win: u32, title: String },
    WinDestroy { win: u32 },
    Frame {
        win: u32,
        seq: u32,
        capture_us: u64,
        width: u32,
        height: u32,
        codec: u8,
        more: bool,
        data: Vec<u8>,
    },
    Pong { echo: u64 },
    PopupCreate { win: u32, parent: u32, x: i32, y: i32, width: u32, height: u32, grab: bool },
    PopupMove { win: u32, x: i32, y: i32, width: u32, height: u32 },
    WinParent { win: u32, parent: u32 },
    WinLimits { win: u32, min_w: u32, min_h: u32, max_w: u32, max_h: u32 },
    Cursor { win: u32, shape: CursorShape },
    Clipboard { mime: String, data: Vec<u8> },
    VideoStart { win: u32, stream: u32, x: u32, y: u32, width: u32, height: u32, codec: u8 },
    VideoFrame { win: u32, stream: u32, seq: u32, capture_us: u64, flags: u8, data: Vec<u8> },
    VideoStop { win: u32, stream: u32 },
    Ping { value: u64 },
    FrameAck { win: u32, seq: u32 },
    PointerMove { win: u32, x: f32, y: f32 },
    PointerButton { win: u32, button: u32, pressed: bool },
    PointerAxis { win: u32, dx: f32, dy: f32 },
    Key { win: u32, keycode: u32, pressed: bool },
    ResizeReq { win: u32, width: u32, height: u32 },
    CloseReq { win: u32 },
    Focus { win: u32, focused: bool },
    ClipboardUp { mime: String, data: Vec<u8> },
    AudioStart { stream: u8, codec: u8, channels: u8, rate: u32, frames: u16 },
    AudioData { stream: u8, flags: u8, seq: u32, pts_us: u64, data: Vec<u8> },
    AudioStop { stream: u8 },
    AudioRequest { stream: u8, on: bool },
    AuthStatus { state: u8, code: [u8; 6] },
    Goodbye { reason: u8, text: String },
    Status { flags: u32, temp_dc: i16 },
}

impl Message {
    pub fn hello(name: &str) -> Message {
        Message::hello_with_caps(name, CAP_BLOCK_CODEC)
    }

    pub fn hello_with_caps(name: &str, caps: u32) -> Message {
        Message::Hello { version: VERSION, caps, name: name.to_string() }
    }

    pub fn kind(&self) -> u16 {
        match self {
            Message::Hello { .. } => kind::HELLO,
            Message::WinCreate { .. } => kind::WIN_CREATE,
            Message::WinResize { .. } => kind::WIN_RESIZE,
            Message::WinTitle { .. } => kind::WIN_TITLE,
            Message::WinDestroy { .. } => kind::WIN_DESTROY,
            Message::Frame { .. } => kind::FRAME,
            Message::Pong { .. } => kind::PONG,
            Message::Ping { .. } => kind::PING,
            Message::FrameAck { .. } => kind::FRAME_ACK,
            Message::PointerMove { .. } => kind::POINTER_MOVE,
            Message::PointerButton { .. } => kind::POINTER_BUTTON,
            Message::PointerAxis { .. } => kind::POINTER_AXIS,
            Message::Key { .. } => kind::KEY,
            Message::ResizeReq { .. } => kind::RESIZE_REQ,
            Message::CloseReq { .. } => kind::CLOSE_REQ,
            Message::Focus { .. } => kind::FOCUS,
            Message::PopupCreate { .. } => kind::POPUP_CREATE,
            Message::PopupMove { .. } => kind::POPUP_MOVE,
            Message::WinParent { .. } => kind::WIN_PARENT,
            Message::WinLimits { .. } => kind::WIN_LIMITS,
            Message::Cursor { .. } => kind::CURSOR,
            Message::Clipboard { .. } => kind::CLIPBOARD,
            Message::ClipboardUp { .. } => kind::CLIPBOARD_UP,
            Message::VideoStart { .. } => kind::VIDEO_START,
            Message::VideoFrame { .. } => kind::VIDEO_FRAME,
            Message::VideoStop { .. } => kind::VIDEO_STOP,
            Message::AudioStart { .. } => kind::AUDIO_START,
            Message::AudioData { .. } => kind::AUDIO_DATA,
            Message::AudioStop { .. } => kind::AUDIO_STOP,
            Message::AudioRequest { .. } => kind::AUDIO_REQUEST,
            Message::AuthStatus { .. } => kind::AUTH_STATUS,
            Message::Goodbye { .. } => kind::GOODBYE,
            Message::Status { .. } => kind::STATUS,
        }
    }

    pub fn encode_body(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        match self {
            Message::Hello { version, caps, name } => {
                put_u32(out, MAGIC);
                put_u16(out, *version);
                put_u16(out, 0);
                put_u32(out, *caps);
                put_str(out, name)?;
            }
            Message::WinCreate { win, width, height, title, app_id } => {
                put_u32(out, *win);
                put_u32(out, *width);
                put_u32(out, *height);
                put_str(out, title)?;
                put_str(out, app_id)?;
            }
            Message::WinResize { win, width, height } | Message::ResizeReq { win, width, height } => {
                put_u32(out, *win);
                put_u32(out, *width);
                put_u32(out, *height);
            }
            Message::WinTitle { win, title } => {
                put_u32(out, *win);
                put_str(out, title)?;
            }
            Message::WinDestroy { win } | Message::CloseReq { win } => put_u32(out, *win),
            Message::Frame { win, seq, capture_us, width, height, codec, data, .. } => {
                put_u32(out, *win);
                put_u32(out, *seq);
                put_u64(out, *capture_us);
                put_u32(out, *width);
                put_u32(out, *height);
                out.push(*codec);
                out.extend_from_slice(data);
            }
            Message::Pong { echo } => put_u64(out, *echo),
            Message::Ping { value } => put_u64(out, *value),
            Message::FrameAck { win, seq } => {
                put_u32(out, *win);
                put_u32(out, *seq);
            }
            Message::PointerMove { win, x, y } => {
                put_u32(out, *win);
                put_f32(out, *x);
                put_f32(out, *y);
            }
            Message::PointerButton { win, button, pressed } => {
                put_u32(out, *win);
                put_u32(out, *button);
                out.push(*pressed as u8);
            }
            Message::PointerAxis { win, dx, dy } => {
                put_u32(out, *win);
                put_f32(out, *dx);
                put_f32(out, *dy);
            }
            Message::Key { win, keycode, pressed } => {
                put_u32(out, *win);
                put_u32(out, *keycode);
                out.push(*pressed as u8);
            }
            Message::Focus { win, focused } => {
                put_u32(out, *win);
                out.push(*focused as u8);
            }
            Message::PopupCreate { win, parent, x, y, width, height, grab } => {
                put_u32(out, *win);
                put_u32(out, *parent);
                put_i32(out, *x);
                put_i32(out, *y);
                put_u32(out, *width);
                put_u32(out, *height);
                out.push(*grab as u8);
            }
            Message::PopupMove { win, x, y, width, height } => {
                put_u32(out, *win);
                put_i32(out, *x);
                put_i32(out, *y);
                put_u32(out, *width);
                put_u32(out, *height);
            }
            Message::WinParent { win, parent } => {
                put_u32(out, *win);
                put_u32(out, *parent);
            }
            Message::WinLimits { win, min_w, min_h, max_w, max_h } => {
                for v in [win, min_w, min_h, max_w, max_h] {
                    put_u32(out, *v);
                }
            }
            Message::Cursor { win, shape } => {
                put_u32(out, *win);
                match shape {
                    CursorShape::Hidden => out.push(0),
                    CursorShape::Named(name) => {
                        out.push(1);
                        put_str(out, name)?;
                    }
                    CursorShape::Image { width, height, hot_x, hot_y, bgra } => {
                        if *width > MAX_CURSOR_SIDE || *height > MAX_CURSOR_SIDE {
                            return Err(ProtoError::Invalid("imagem de cursor maior que 128x128"));
                        }
                        if bgra.len() != usize::from(*width) * usize::from(*height) * 4 {
                            return Err(ProtoError::Invalid("tamanho dos pixels do cursor"));
                        }
                        out.push(2);
                        put_u16(out, *width);
                        put_u16(out, *height);
                        out.extend_from_slice(&hot_x.to_le_bytes());
                        out.extend_from_slice(&hot_y.to_le_bytes());
                        out.extend_from_slice(bgra);
                    }
                }
            }
            Message::Clipboard { mime, data } | Message::ClipboardUp { mime, data } => {
                if data.len() > MAX_CLIPBOARD {
                    return Err(ProtoError::Invalid("texto da área de transferência acima de 1 MiB"));
                }
                put_str(out, mime)?;
                put_u32(out, data.len() as u32);
                out.extend_from_slice(data);
            }
            Message::VideoStart { win, stream, x, y, width, height, codec } => {
                for v in [win, stream, x, y, width, height] {
                    put_u32(out, *v);
                }
                out.push(*codec);
            }
            Message::VideoFrame { win, stream, seq, capture_us, flags, data } => {
                put_u32(out, *win);
                put_u32(out, *stream);
                put_u32(out, *seq);
                put_u64(out, *capture_us);
                out.push(*flags);
                out.extend_from_slice(data);
            }
            Message::VideoStop { win, stream } => {
                put_u32(out, *win);
                put_u32(out, *stream);
            }
            Message::AudioStart { stream, codec, channels, rate, frames } => {
                if *channels != 1 && *channels != 2 {
                    return Err(ProtoError::Invalid("canais de áudio devem ser 1 ou 2"));
                }
                out.push(*stream);
                out.push(*codec);
                out.push(*channels);
                out.push(0);
                put_u32(out, *rate);
                put_u16(out, *frames);
                put_u16(out, 0);
            }
            Message::AudioData { stream, flags, seq, pts_us, data } => {
                if data.len() > MAX_AUDIO_PAYLOAD {
                    return Err(ProtoError::Invalid("carga de áudio acima de 64 KiB"));
                }
                out.push(*stream);
                out.push(*flags);
                put_u16(out, 0);
                put_u32(out, *seq);
                put_u64(out, *pts_us);
                out.extend_from_slice(data);
            }
            Message::AudioStop { stream } => {
                out.push(*stream);
                out.extend_from_slice(&[0, 0, 0]);
            }
            Message::AudioRequest { stream, on } => {
                out.push(*stream);
                out.push(u8::from(*on));
                out.extend_from_slice(&[0, 0]);
            }
            Message::AuthStatus { state, code } => {
                out.push(*state);
                out.extend_from_slice(&[0, 0, 0]);
                out.extend_from_slice(code);
            }
            Message::Goodbye { reason, text } => {
                out.push(*reason);
                put_str(out, cut_text(text, MAX_GOODBYE_TEXT))?;
            }
            Message::Status { flags, temp_dc } => {
                put_u32(out, *flags);
                out.extend_from_slice(&temp_dc.to_le_bytes());
            }
        }
        Ok(())
    }

    pub fn decode_body(kind_code: u16, body: &[u8]) -> Result<Option<Message>, ProtoError> {
        Message::decode_body_with_flags(kind_code, 0, body)
    }

    pub fn decode_body_with_flags(kind_code: u16, header_flags: u16, body: &[u8]) -> Result<Option<Message>, ProtoError> {
        let mut r = Cursor { buf: body, pos: 0 };
        let msg = match kind_code {
            kind::HELLO => {
                let magic = r.u32()?;
                if magic != MAGIC {
                    return Err(ProtoError::BadMagic(magic));
                }
                let version = r.u16()?;
                let _reserved = r.u16()?;
                let caps = r.u32()?;
                let name = r.string()?;
                Message::Hello { version, caps, name }
            }
            kind::WIN_CREATE => Message::WinCreate {
                win: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                title: r.string()?,
                app_id: r.string()?,
            },
            kind::WIN_RESIZE => Message::WinResize { win: r.u32()?, width: r.u32()?, height: r.u32()? },
            kind::WIN_TITLE => Message::WinTitle { win: r.u32()?, title: r.string()? },
            kind::WIN_DESTROY => Message::WinDestroy { win: r.u32()? },
            kind::FRAME => {
                let win = r.u32()?;
                let seq = r.u32()?;
                let capture_us = r.u64()?;
                let width = r.u32()?;
                let height = r.u32()?;
                let codec = r.u8()?;
                let data = r.rest().to_vec();
                Message::Frame { win, seq, capture_us, width, height, codec, more: header_flags & FRAME_FLAG_MORE != 0, data }
            }
            kind::PONG => Message::Pong { echo: r.u64()? },
            kind::PING => Message::Ping { value: r.u64()? },
            kind::FRAME_ACK => Message::FrameAck { win: r.u32()?, seq: r.u32()? },
            kind::POINTER_MOVE => Message::PointerMove { win: r.u32()?, x: r.f32()?, y: r.f32()? },
            kind::POINTER_BUTTON => {
                Message::PointerButton { win: r.u32()?, button: r.u32()?, pressed: r.u8()? != 0 }
            }
            kind::POINTER_AXIS => Message::PointerAxis { win: r.u32()?, dx: r.f32()?, dy: r.f32()? },
            kind::KEY => Message::Key { win: r.u32()?, keycode: r.u32()?, pressed: r.u8()? != 0 },
            kind::RESIZE_REQ => Message::ResizeReq { win: r.u32()?, width: r.u32()?, height: r.u32()? },
            kind::CLOSE_REQ => Message::CloseReq { win: r.u32()? },
            kind::FOCUS => Message::Focus { win: r.u32()?, focused: r.u8()? != 0 },
            kind::POPUP_CREATE => Message::PopupCreate {
                win: r.u32()?,
                parent: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.u32()?,
                height: r.u32()?,
                grab: r.u8()? != 0,
            },
            kind::POPUP_MOVE => Message::PopupMove { win: r.u32()?, x: r.i32()?, y: r.i32()?, width: r.u32()?, height: r.u32()? },
            kind::WIN_PARENT => Message::WinParent { win: r.u32()?, parent: r.u32()? },
            kind::WIN_LIMITS => Message::WinLimits { win: r.u32()?, min_w: r.u32()?, min_h: r.u32()?, max_w: r.u32()?, max_h: r.u32()? },
            kind::CURSOR => {
                let win = r.u32()?;
                let shape = match r.u8()? {
                    0 => CursorShape::Hidden,
                    1 => CursorShape::Named(r.string()?),
                    2 => {
                        let width = r.u16()?;
                        let height = r.u16()?;
                        let hot_x = r.u16()? as i16;
                        let hot_y = r.u16()? as i16;
                        if width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
                            return Err(ProtoError::Invalid("imagem de cursor maior que 128x128"));
                        }
                        let bgra = r.take(usize::from(width) * usize::from(height) * 4)?.to_vec();
                        CursorShape::Image { width, height, hot_x, hot_y, bgra }
                    }
                    _ => return Err(ProtoError::Invalid("tipo de cursor desconhecido")),
                };
                Message::Cursor { win, shape }
            }
            kind::CLIPBOARD | kind::CLIPBOARD_UP => {
                let mime = r.string()?;
                let len = r.u32()? as usize;
                if len > MAX_CLIPBOARD {
                    return Err(ProtoError::Invalid("texto da área de transferência acima de 1 MiB"));
                }
                let data = r.take(len)?.to_vec();
                if kind_code == kind::CLIPBOARD {
                    Message::Clipboard { mime, data }
                } else {
                    Message::ClipboardUp { mime, data }
                }
            }
            kind::VIDEO_START => Message::VideoStart {
                win: r.u32()?,
                stream: r.u32()?,
                x: r.u32()?,
                y: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                codec: r.u8()?,
            },
            kind::VIDEO_FRAME => {
                let win = r.u32()?;
                let stream = r.u32()?;
                let seq = r.u32()?;
                let capture_us = r.u64()?;
                let flags = r.u8()?;
                let data = r.rest().to_vec();
                Message::VideoFrame { win, stream, seq, capture_us, flags, data }
            }
            kind::VIDEO_STOP => Message::VideoStop { win: r.u32()?, stream: r.u32()? },
            kind::AUDIO_START => {
                let stream = r.u8()?;
                let codec = r.u8()?;
                let channels = r.u8()?;
                let _reserved = r.u8()?;
                let rate = r.u32()?;
                let frames = r.u16()?;
                let _reserved = r.u16()?;
                if channels != 1 && channels != 2 {
                    return Err(ProtoError::Invalid("canais de áudio devem ser 1 ou 2"));
                }
                Message::AudioStart { stream, codec, channels, rate, frames }
            }
            kind::AUDIO_DATA => {
                let stream = r.u8()?;
                let flags = r.u8()?;
                let _reserved = r.u16()?;
                let seq = r.u32()?;
                let pts_us = r.u64()?;
                let data = r.rest();
                if data.len() > MAX_AUDIO_PAYLOAD {
                    return Err(ProtoError::Invalid("carga de áudio acima de 64 KiB"));
                }
                Message::AudioData { stream, flags, seq, pts_us, data: data.to_vec() }
            }
            kind::AUDIO_STOP => {
                let stream = r.u8()?;
                let _reserved = r.take(3)?;
                Message::AudioStop { stream }
            }
            kind::AUDIO_REQUEST => {
                let stream = r.u8()?;
                let on = r.u8()?;
                let _reserved = r.take(2)?;
                if on > 1 {
                    return Err(ProtoError::Invalid("AUDIO_REQUEST: `on` deve ser 0 ou 1"));
                }
                Message::AudioRequest { stream, on: on == 1 }
            }
            kind::AUTH_STATUS => {
                let state = r.u8()?;
                let _reserved = r.take(3)?;
                let mut code = [0u8; 6];
                code.copy_from_slice(r.take(6)?);
                if state > AUTH_REFUSED {
                    return Err(ProtoError::Invalid("estado de pareamento desconhecido"));
                }
                Message::AuthStatus { state, code }
            }
            kind::GOODBYE => {
                let reason = r.u8()?;
                let text = r.string()?;
                Message::Goodbye { reason, text }
            }
            kind::STATUS => {
                let flags = r.u32()?;
                let b = r.take(2)?;
                Message::Status { flags, temp_dc: i16::from_le_bytes([b[0], b[1]]) }
            }
            _ => return Ok(None),
        };
        Ok(Some(msg))
    }
}

pub fn encode_message(msg: &Message, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    let start = out.len();
    out.extend_from_slice(&[0u8; HEADER_LEN]);
    msg.encode_body(out)?;
    let body_len = out.len() - start - HEADER_LEN;
    if body_len > MAX_BODY_LEN as usize {
        out.truncate(start);
        return Err(ProtoError::TooLarge(u32::try_from(body_len).unwrap_or(u32::MAX)));
    }
    out[start..start + 4].copy_from_slice(&(body_len as u32).to_le_bytes());
    out[start + 4..start + 6].copy_from_slice(&msg.kind().to_le_bytes());
    if let Message::Frame { more: true, .. } = msg {
        out[start + 6..start + 8].copy_from_slice(&FRAME_FLAG_MORE.to_le_bytes());
    }
    Ok(())
}

pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), ProtoError> {
    let mut buf = Vec::with_capacity(HEADER_LEN + 64);
    encode_message(msg, &mut buf)?;
    w.write_all(&buf)?;
    Ok(())
}

pub fn read_message<R: Read>(r: &mut R) -> Result<Message, ProtoError> {
    let mut body = Vec::new();
    loop {
        let mut header = [0u8; HEADER_LEN];
        match read_full_or_eof(r, &mut header)? {
            Fill::Eof => return Err(ProtoError::Closed),
            Fill::Full => {}
        }
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let kind_code = u16::from_le_bytes([header[4], header[5]]);
        let header_flags = u16::from_le_bytes([header[6], header[7]]);
        if len > MAX_BODY_LEN {
            return Err(ProtoError::TooLarge(len));
        }
        if !is_known_kind(kind_code) {
            skip_bytes(r, len as u64)?;
            continue;
        }
        body.clear();
        body.resize(len as usize, 0);
        r.read_exact(&mut body).map_err(map_eof)?;
        match Message::decode_body_with_flags(kind_code, header_flags, &body)? {
            Some(m) => return Ok(m),
            None => continue,
        }
    }
}

pub fn read_hello<R: Read>(r: &mut R) -> Result<(u32, String), ProtoError> {
    match read_message(r)? {
        Message::Hello { version, caps, name } => {
            if version != VERSION {
                return Err(ProtoError::BadVersion(version));
            }
            Ok((caps, name))
        }
        outra => Err(ProtoError::Unexpected(outra.kind())),
    }
}

fn is_known_kind(k: u16) -> bool {
    matches!(
        k,
        kind::HELLO
            | kind::AUDIO_START
            | kind::AUDIO_DATA
            | kind::AUDIO_STOP
            | kind::AUDIO_REQUEST
            | kind::AUTH_STATUS
            | kind::GOODBYE
            | kind::STATUS
            | kind::WIN_CREATE
            | kind::WIN_RESIZE
            | kind::WIN_TITLE
            | kind::WIN_DESTROY
            | kind::FRAME
            | kind::PING
            | kind::PONG
            | kind::FRAME_ACK
            | kind::POINTER_MOVE
            | kind::POINTER_BUTTON
            | kind::POINTER_AXIS
            | kind::KEY
            | kind::RESIZE_REQ
            | kind::CLOSE_REQ
            | kind::FOCUS
            | kind::POPUP_CREATE
            | kind::CURSOR
            | kind::CLIPBOARD
            | kind::WIN_PARENT
            | kind::WIN_LIMITS
            | kind::POPUP_MOVE
            | kind::CLIPBOARD_UP
            | kind::VIDEO_START
            | kind::VIDEO_FRAME
            | kind::VIDEO_STOP
    )
}

fn map_eof(e: io::Error) -> ProtoError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ProtoError::Truncated
    } else {
        ProtoError::Io(e)
    }
}

enum Fill {
    Full,
    Eof,
}

fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<Fill, ProtoError> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => {
                return if got == 0 { Ok(Fill::Eof) } else { Err(ProtoError::Truncated) };
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ProtoError::Io(e)),
        }
    }
    Ok(Fill::Full)
}

fn skip_bytes<R: Read>(r: &mut R, mut n: u64) -> Result<(), ProtoError> {
    let mut scratch = [0u8; 4096];
    while n > 0 {
        let take = n.min(scratch.len() as u64) as usize;
        r.read_exact(&mut scratch[..take]).map_err(map_eof)?;
        n -= take as u64;
    }
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_str(out: &mut Vec<u8>, s: &str) -> Result<(), ProtoError> {
    let len = u16::try_from(s.len()).map_err(|_| ProtoError::StringTooLong(s.len()))?;
    put_u16(out, len);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn cut_text(t: &str, max: usize) -> &str {
    let mut fim = t.len().min(max);
    while !t.is_char_boundary(fim) {
        fim -= 1;
    }
    &t[..fim]
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::Truncated)?;
        if end > self.buf.len() {
            return Err(ProtoError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(self.u32()? as i32)
    }
    fn f32(&mut self) -> Result<f32, ProtoError> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn string(&mut self) -> Result<String, ProtoError> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| ProtoError::BadUtf8)
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todas() -> Vec<Message> {
        vec![
            Message::hello("psd 0.1 ç"),
            Message::WinCreate {
                win: 7,
                width: 800,
                height: 600,
                title: "Relógio — teste".into(),
                app_id: "org.pistick.teste".into(),
            },
            Message::WinResize { win: 7, width: 1, height: 4096 },
            Message::WinTitle { win: 9, title: String::new() },
            Message::WinDestroy { win: 3 },
            Message::Frame {
                win: 2,
                seq: 99,
                capture_us: 0x0123_4567_89AB_CDEF,
                width: 64,
                height: 32,
                codec: CODEC_BLOCKS,
                more: false,
                data: vec![1, 2, 3, 0, 255],
            },
            Message::Frame {
                win: 2,
                seq: 100,
                capture_us: 1,
                width: 1,
                height: 1,
                codec: 1,
                more: true,
                data: vec![],
            },
            Message::Pong { echo: u64::MAX },
            Message::Ping { value: 42 },
            Message::FrameAck { win: 5, seq: 6 },
            Message::PointerMove { win: 1, x: 12.5, y: -3.25 },
            Message::PointerButton { win: 1, button: 0x110, pressed: true },
            Message::PointerButton { win: 1, button: 0x111, pressed: false },
            Message::PointerAxis { win: 1, dx: 0.0, dy: -15.0 },
            Message::Key { win: 1, keycode: 30, pressed: true },
            Message::ResizeReq { win: 4, width: 640, height: 480 },
            Message::CloseReq { win: 4 },
            Message::Focus { win: 4, focused: true },
            Message::Focus { win: 4, focused: false },
            Message::PopupCreate { win: 8, parent: 7, x: -12, y: 345, width: 200, height: 300, grab: true },
            Message::PopupMove { win: 8, x: 5, y: -6, width: 10, height: 20 },
            Message::WinParent { win: 9, parent: 7 },
            Message::WinParent { win: 9, parent: 0 },
            Message::WinLimits { win: 7, min_w: 100, min_h: 50, max_w: 0, max_h: 0 },
            Message::Cursor { win: 7, shape: CursorShape::Hidden },
            Message::Cursor { win: 7, shape: CursorShape::Named("text".into()) },
            Message::Cursor { win: 7, shape: CursorShape::Image { width: 2, height: 1, hot_x: -1, hot_y: 3, bgra: vec![1, 2, 3, 4, 5, 6, 7, 8] } },
            Message::Clipboard { mime: MIME_TEXT.into(), data: "área ç".as_bytes().to_vec() },
            Message::Clipboard { mime: MIME_TEXT.into(), data: vec![] },
            Message::ClipboardUp { mime: MIME_TEXT.into(), data: b"pistick-teste-123".to_vec() },
            Message::VideoStart { win: 7, stream: 3, x: 64, y: 128, width: 640, height: 384, codec: CODEC_H264 },
            Message::VideoFrame {
                win: 7,
                stream: 3,
                seq: 1234,
                capture_us: 0x0123_4567_89AB_CDEF,
                flags: VIDEO_FLAG_KEY,
                data: vec![0, 0, 0, 1, 0x67, 0xFF],
            },
            Message::VideoFrame { win: 7, stream: 3, seq: 1235, capture_us: 9, flags: 0, data: vec![] },
            Message::VideoStop { win: 7, stream: 3 },
            Message::AudioStart { stream: AUDIO_STREAM_OUT, codec: 1, channels: 2, rate: 48000, frames: 480 },
            Message::AudioStart { stream: AUDIO_STREAM_MIC, codec: 0, channels: 1, rate: 48000, frames: 960 },
            Message::AudioData { stream: 0, flags: AUDIO_FLAG_DISCONTINUITY, seq: 7, pts_us: 0x0123_4567_89AB_CDEF, data: vec![1, 2, 3] },
            Message::AudioData { stream: 1, flags: 0, seq: 8, pts_us: 1, data: vec![] },
            Message::AudioStop { stream: 0 },
            Message::AudioRequest { stream: AUDIO_STREAM_MIC, on: true },
            Message::AudioRequest { stream: AUDIO_STREAM_MIC, on: false },
            Message::AuthStatus { state: AUTH_PAIRED, code: [0; 6] },
            Message::AuthStatus { state: AUTH_PENDING, code: *b"042917" },
            Message::Goodbye { reason: GOODBYE_REPLACED, text: "notebook de ação".into() },
            Message::Goodbye { reason: GOODBYE_CLIENT, text: String::new() },
            Message::Goodbye { reason: GOODBYE_SHUTDOWN, text: String::new() },
            Message::Status { flags: STATUS_UNDERVOLT_NOW | STATUS_DISPOSABLE, temp_dc: 487 },
            Message::Status { flags: 0, temp_dc: STATUS_TEMP_UNKNOWN },
            Message::Status { flags: u32::MAX, temp_dc: -125 },
        ]
    }

    #[test]
    fn goodbye_e_status_tem_o_formato_do_fio() {
        assert_eq!(CAP_SESSION, 0x100);
        let mut buf = Vec::new();
        encode_message(&Message::Goodbye { reason: GOODBYE_REPLACED, text: "ab".into() }, &mut buf).unwrap();
        assert_eq!(buf, vec![5, 0, 0, 0, 46, 0, 0, 0, 1, 2, 0, b'a', b'b']);
        buf.clear();
        encode_message(&Message::Goodbye { reason: GOODBYE_CLIENT, text: String::new() }, &mut buf).unwrap();
        assert_eq!(buf, vec![3, 0, 0, 0, 46, 0, 0, 0, 0, 0, 0]);
        buf.clear();
        encode_message(&Message::Status { flags: STATUS_UNDERVOLT_NOW | STATUS_THROTTLED_NOW, temp_dc: 487 }, &mut buf).unwrap();
        assert_eq!(buf, vec![6, 0, 0, 0, 47, 0, 0, 0, 5, 0, 0, 0, 0xE7, 0x01]);
        buf.clear();
        encode_message(&Message::Status { flags: 0, temp_dc: STATUS_TEMP_UNKNOWN }, &mut buf).unwrap();
        assert_eq!(&buf[8..], &[0, 0, 0, 0, 0x00, 0x80]);
        assert!(matches!(Message::decode_body(kind::STATUS, &[0, 0, 0, 0, 1]), Err(ProtoError::Truncated)));
        assert!(matches!(Message::decode_body(kind::GOODBYE, &[1, 5, 0, b'a']), Err(ProtoError::Truncated)));
        assert!(matches!(Message::decode_body(kind::GOODBYE, &[1, 1, 0, 0xFF]), Err(ProtoError::BadUtf8)));
    }

    #[test]
    fn goodbye_corta_o_texto_longo_em_fronteira_de_caractere() {
        let longo = "ç".repeat(200);
        let mut buf = Vec::new();
        encode_message(&Message::Goodbye { reason: GOODBYE_REPLACED, text: longo }, &mut buf).unwrap();
        let Message::Goodbye { text, .. } = read_message(&mut buf.as_slice()).unwrap() else { panic!("não é GOODBYE") };
        assert!(text.chars().all(|c| c == 'ç'));
        assert_eq!(text.len(), 254);
    }

    #[test]
    fn corpos_aleatorios_de_goodbye_e_status_nao_entram_em_panico() {
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..20_000 {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let n = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 59) as usize;
            let body: Vec<u8> = (0..n).map(|i| (x >> (i % 56)) as u8).collect();
            for k in [kind::GOODBYE, kind::STATUS] {
                let _ = Message::decode_body(k, &body);
            }
        }
    }

    #[test]
    fn auth_status_com_estado_desconhecido_e_recusado() {
        let mut corpo = vec![9u8, 0, 0, 0];
        corpo.extend_from_slice(b"000000");
        assert!(matches!(Message::decode_body(kind::AUTH_STATUS, &corpo), Err(ProtoError::Invalid(_))));
        assert!(matches!(Message::decode_body(kind::AUTH_STATUS, &corpo[..9]), Err(ProtoError::Truncated)));
    }

    #[test]
    fn ida_e_volta_de_cada_mensagem() {
        for m in todas() {
            let mut buf = Vec::new();
            encode_message(&m, &mut buf).unwrap();
            let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            assert_eq!(len, buf.len() - HEADER_LEN, "len de {m:?}");
            assert_eq!(u16::from_le_bytes(buf[4..6].try_into().unwrap()), m.kind());
            let esperado: &[u8] = if matches!(m, Message::Frame { more: true, .. }) { &[1, 0] } else { &[0, 0] };
            assert_eq!(&buf[6..8], esperado, "flags");
            let lida = read_message(&mut buf.as_slice()).unwrap();
            assert_eq!(lida, m);
        }
    }

    #[test]
    fn sequencia_de_mensagens_no_mesmo_fluxo() {
        let mut fluxo = Vec::new();
        for m in todas() {
            write_message(&mut fluxo, &m).unwrap();
        }
        let mut r = fluxo.as_slice();
        for m in todas() {
            assert_eq!(read_message(&mut r).unwrap(), m);
        }
        assert!(matches!(read_message(&mut r), Err(ProtoError::Closed)));
    }

    #[test]
    fn bytes_exatos_do_hello() {
        let mut buf = Vec::new();
        encode_message(&Message::hello("ab"), &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            16, 0, 0, 0,
            1, 0,
            0, 0,
            b'P', b'S', b'T', b'K',
            1, 0,
            0, 0,
            1, 0, 0, 0,
            2, 0, b'a', b'b',
        ];
        assert_eq!(buf, esperado);
    }

    #[test]
    fn frame_com_more_leva_o_bit_0_no_cabecalho_e_so_ele() {
        let frame = |more| Message::Frame { win: 1, seq: 2, capture_us: 3, width: 4, height: 5, codec: CODEC_BLOCKS, more, data: vec![9] };
        let mut buf = Vec::new();
        encode_message(&frame(true), &mut buf).unwrap();
        assert_eq!(&buf[6..8], &[1, 0], "FRAME_FLAG_MORE no campo flags do cabeçalho");
        assert_eq!(read_message(&mut buf.as_slice()).unwrap(), frame(true));
        buf.clear();
        encode_message(&frame(false), &mut buf).unwrap();
        assert_eq!(&buf[6..8], &[0, 0], "sem MORE o campo flags continua 0");
        assert_eq!(read_message(&mut buf.as_slice()).unwrap(), frame(false));
        buf.clear();
        encode_message(&Message::Ping { value: 1 }, &mut buf).unwrap();
        assert_eq!(&buf[6..8], &[0, 0]);
        buf.clear();
        encode_message(&frame(false), &mut buf).unwrap();
        buf[6] = 0xFE;
        assert_eq!(read_message(&mut buf.as_slice()).unwrap(), frame(false));
        assert_eq!(CAP_ATOMIC, 1 << 6);
    }

    #[test]
    fn kind_desconhecido_e_pulado() {
        let mut fluxo = Vec::new();
        write_message(&mut fluxo, &Message::Ping { value: 1 }).unwrap();
        fluxo.extend_from_slice(&10_000u32.to_le_bytes());
        fluxo.extend_from_slice(&999u16.to_le_bytes());
        fluxo.extend_from_slice(&0u16.to_le_bytes());
        fluxo.extend(std::iter::repeat_n(0xAB, 10_000));
        write_message(&mut fluxo, &Message::Ping { value: 2 }).unwrap();
        let mut r = fluxo.as_slice();
        assert_eq!(read_message(&mut r).unwrap(), Message::Ping { value: 1 });
        assert_eq!(read_message(&mut r).unwrap(), Message::Ping { value: 2 });
    }

    #[test]
    fn kind_desconhecido_truncado_e_erro() {
        let mut fluxo = Vec::new();
        fluxo.extend_from_slice(&100u32.to_le_bytes());
        fluxo.extend_from_slice(&999u16.to_le_bytes());
        fluxo.extend_from_slice(&0u16.to_le_bytes());
        fluxo.extend_from_slice(&[0; 10]);
        assert!(matches!(read_message(&mut fluxo.as_slice()), Err(ProtoError::Truncated)));
    }

    #[test]
    fn len_acima_do_maximo_e_rejeitado() {
        let mut fluxo = Vec::new();
        fluxo.extend_from_slice(&(MAX_BODY_LEN + 1).to_le_bytes());
        fluxo.extend_from_slice(&kind::FRAME.to_le_bytes());
        fluxo.extend_from_slice(&0u16.to_le_bytes());
        assert!(matches!(read_message(&mut fluxo.as_slice()), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    fn corpo_truncado_e_erro_sem_panico() {
        for m in todas() {
            let mut body = Vec::new();
            m.encode_body(&mut body).unwrap();
            for corte in 0..body.len() {
                let r = Message::decode_body(m.kind(), &body[..corte]);
                let fixo = match m {
                    Message::Frame { .. } => Some(25),
                    Message::VideoFrame { .. } => Some(21),
                    Message::AudioData { .. } => Some(16),
                    _ => None,
                };
                if fixo.is_some_and(|f| corte >= f) {
                    assert!(r.is_ok());
                    continue;
                }
                assert!(matches!(r, Err(ProtoError::Truncated)), "{m:?} corte {corte}: {r:?}");
            }
        }
    }

    #[test]
    fn cabecalho_truncado_e_erro_e_vazio_e_closed() {
        assert!(matches!(read_message(&mut [].as_slice()), Err(ProtoError::Closed)));
        assert!(matches!(read_message(&mut [1u8, 0, 0].as_slice()), Err(ProtoError::Truncated)));
    }

    #[test]
    fn hello_com_magic_errado() {
        let mut body = Vec::new();
        Message::hello("x").encode_body(&mut body).unwrap();
        body[0] ^= 0xFF;
        assert!(matches!(
            Message::decode_body(kind::HELLO, &body),
            Err(ProtoError::BadMagic(_))
        ));
    }

    #[test]
    fn hello_com_versao_errada() {
        let mut fluxo = Vec::new();
        write_message(&mut fluxo, &Message::Hello { version: 2, caps: 0, name: "x".into() }).unwrap();
        assert!(matches!(read_hello(&mut fluxo.as_slice()), Err(ProtoError::BadVersion(2))));
    }

    #[test]
    fn bytes_exatos_do_popup_create() {
        let mut buf = Vec::new();
        let m = Message::PopupCreate { win: 1, parent: 2, x: -1, y: 3, width: 4, height: 5, grab: true };
        encode_message(&m, &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            25, 0, 0, 0, 21, 0, 0, 0,
            1, 0, 0, 0, 2, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 1,
        ];
        assert_eq!(buf, esperado);
    }

    #[test]
    fn cursor_e_area_de_transferencia_invalidos() {
        let grande = Message::Cursor { win: 1, shape: CursorShape::Image { width: 129, height: 1, hot_x: 0, hot_y: 0, bgra: vec![0; 129 * 4] } };
        assert!(matches!(encode_message(&grande, &mut Vec::new()), Err(ProtoError::Invalid(_))));
        let incoerente = Message::Cursor { win: 1, shape: CursorShape::Image { width: 2, height: 2, hot_x: 0, hot_y: 0, bgra: vec![0; 3] } };
        assert!(matches!(encode_message(&incoerente, &mut Vec::new()), Err(ProtoError::Invalid(_))));
        let enorme = Message::Clipboard { mime: MIME_TEXT.into(), data: vec![b'a'; MAX_CLIPBOARD + 1] };
        assert!(matches!(encode_message(&enorme, &mut Vec::new()), Err(ProtoError::Invalid(_))));
        let mut corpo = Vec::new();
        put_str(&mut corpo, MIME_TEXT).unwrap();
        put_u32(&mut corpo, (MAX_CLIPBOARD + 1) as u32);
        assert!(matches!(Message::decode_body(kind::CLIPBOARD, &corpo), Err(ProtoError::Invalid(_))));
        assert!(matches!(Message::decode_body(kind::CURSOR, &[1, 0, 0, 0, 9]), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn utf8_invalido_e_erro() {
        let mut body = Vec::new();
        Message::WinTitle { win: 1, title: "abc".into() }.encode_body(&mut body).unwrap();
        let n = body.len();
        body[n - 1] = 0xFF;
        assert!(matches!(Message::decode_body(kind::WIN_TITLE, &body), Err(ProtoError::BadUtf8)));
    }

    #[test]
    fn texto_longo_demais() {
        let m = Message::WinTitle { win: 1, title: "a".repeat(70_000) };
        assert!(matches!(encode_message(&m, &mut Vec::new()), Err(ProtoError::StringTooLong(_))));
    }

    #[test]
    fn bytes_exatos_das_mensagens_de_video() {
        let mut buf = Vec::new();
        let m = Message::VideoStart { win: 1, stream: 2, x: 64, y: 128, width: 256, height: 320, codec: CODEC_H264 };
        encode_message(&m, &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            25, 0, 0, 0, 27, 0, 0, 0,
            1, 0, 0, 0, 2, 0, 0, 0, 64, 0, 0, 0, 128, 0, 0, 0, 0, 1, 0, 0, 64, 1, 0, 0, 1,
        ];
        assert_eq!(buf, esperado);

        let mut buf = Vec::new();
        let m = Message::VideoFrame { win: 1, stream: 2, seq: 3, capture_us: 4, flags: 1, data: vec![9, 8] };
        encode_message(&m, &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            23, 0, 0, 0, 28, 0, 0, 0,
            1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 1, 9, 8,
        ];
        assert_eq!(buf, esperado);

        let mut buf = Vec::new();
        encode_message(&Message::VideoStop { win: 1, stream: 2 }, &mut buf).unwrap();
        assert_eq!(buf, vec![8, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]);
    }

    #[test]
    fn bytes_exatos_das_mensagens_de_audio() {
        let mut buf = Vec::new();
        let m = Message::AudioStart { stream: 0, codec: 1, channels: 2, rate: 48000, frames: 480 };
        encode_message(&m, &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            12, 0, 0, 0, 41, 0, 0, 0,
            0, 1, 2, 0, 0x80, 0xBB, 0, 0, 0xE0, 0x01, 0, 0,
        ];
        assert_eq!(buf, esperado);

        let mut buf = Vec::new();
        let m = Message::AudioData { stream: 0, flags: 1, seq: 3, pts_us: 4, data: vec![9, 8] };
        encode_message(&m, &mut buf).unwrap();
        let esperado: Vec<u8> = vec![
            18, 0, 0, 0, 42, 0, 0, 0,
            0, 1, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 9, 8,
        ];
        assert_eq!(buf, esperado);

        let mut buf = Vec::new();
        encode_message(&Message::AudioStop { stream: 1 }, &mut buf).unwrap();
        assert_eq!(buf, vec![4, 0, 0, 0, 43, 0, 0, 0, 1, 0, 0, 0]);

        let mut buf = Vec::new();
        encode_message(&Message::AudioRequest { stream: 1, on: true }, &mut buf).unwrap();
        assert_eq!(buf, vec![4, 0, 0, 0, 45, 0, 0, 0, 1, 1, 0, 0]);
        assert!(matches!(Message::decode_body(kind::AUDIO_REQUEST, &[1, 2, 0, 0]), Err(ProtoError::Invalid(_))));
        assert_eq!(CAP_MIC, 0x80);
    }

    #[test]
    fn audio_invalido_e_recusado() {
        let ruim = Message::AudioStart { stream: 0, codec: 0, channels: 3, rate: 48000, frames: 480 };
        assert!(matches!(encode_message(&ruim, &mut Vec::new()), Err(ProtoError::Invalid(_))));
        let grande = Message::AudioData { stream: 0, flags: 0, seq: 1, pts_us: 0, data: vec![0; MAX_AUDIO_PAYLOAD + 1] };
        assert!(matches!(encode_message(&grande, &mut Vec::new()), Err(ProtoError::Invalid(_))));
        let ok = Message::AudioData { stream: 0, flags: 0, seq: 1, pts_us: 0, data: vec![0; MAX_AUDIO_PAYLOAD] };
        assert!(encode_message(&ok, &mut Vec::new()).is_ok());
        assert!(matches!(
            Message::decode_body(kind::AUDIO_START, &[0, 0, 0, 0, 0x80, 0xBB, 0, 0, 0xE0, 1, 0, 0]),
            Err(ProtoError::Invalid(_))
        ));
        let mut corpo = vec![0u8; 16];
        corpo.extend(std::iter::repeat_n(0u8, MAX_AUDIO_PAYLOAD + 1));
        assert!(matches!(Message::decode_body(kind::AUDIO_DATA, &corpo), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn corpos_aleatorios_de_audio_nao_entram_em_panico() {
        let mut x = 0x0123_4567_89AB_CDEF_u64;
        for _ in 0..20_000 {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let n = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 58) as usize;
            let body: Vec<u8> = (0..n).map(|i| (x >> (i % 56)) as u8).collect();
            for k in [kind::AUDIO_START, kind::AUDIO_DATA, kind::AUDIO_STOP] {
                let _ = Message::decode_body(k, &body);
            }
        }
    }
}
