//! Sonda do `psd`: cliente do protocolo sem janela, para testes manuais e medições.
//!
//! Conecta, decodifica e confirma todos os quadros, e imprime por segundo quantos quadros e
//! bytes chegaram. Pode pedir redimensionamento e fechamento da primeira janela.
//!
//! Uso: `cargo run --release -p ps-server --example probe -- ENDEREÇO [--seconds N]
//!        [--resize LxA] [--close] [--hold-ack]`

use std::time::{Duration, Instant};

use ps_codec::Decoder;
use ps_proto::{read_hello, read_message, write_message, Message, ProtoError};
use ps_transport::{Split, TcpTransport, Transport};

fn main() {
    let mut addr = None;
    let mut seconds = 5u64;
    let mut resize: Option<(u32, u32)> = None;
    let mut close = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).expect("--seconds N"),
            "--resize" => {
                let v = args.next().expect("--resize LxA");
                let (l, h) = v.split_once('x').expect("--resize LxA");
                resize = Some((l.parse().expect("largura"), h.parse().expect("altura")));
            }
            "--close" => close = true,
            outro => addr = Some(outro.to_string()),
        }
    }
    let addr = addr.expect("uso: probe ENDEREÇO [--seconds N] [--resize LxA] [--close]");
    let Split { mut reader, mut writer, closer: _c } = TcpTransport::connect(&addr).expect("conectar").split().unwrap();
    write_message(&mut writer, &Message::hello("probe")).unwrap();
    let (_, name) = read_hello(&mut reader).expect("HELLO");
    println!("conectado a {addr}: \"{name}\"");

    let (tx, rx) = std::sync::mpsc::channel::<Result<Message, ProtoError>>();
    std::thread::spawn(move || loop {
        let r = read_message(&mut reader);
        let fim = r.is_err();
        if tx.send(r).is_err() || fim {
            break;
        }
    });

    let mut decoders: std::collections::HashMap<u32, Decoder> = Default::default();
    let t0 = Instant::now();
    let mut last = t0;
    let (mut frames, mut bytes) = (0u32, 0usize);
    let mut first_win = None;
    let mut sent_resize = false;
    let mut sent_close = false;
    let mut resized_at = None::<Instant>;
    let mut closed_ok = false;
    while t0.elapsed() < Duration::from_secs(seconds) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(m)) => match m {
                Message::WinCreate { win, width, height, title, app_id } => {
                    println!("+{:.2}s WIN_CREATE {win} {width}x{height} título={title:?} app_id={app_id:?}", t0.elapsed().as_secs_f32());
                    decoders.insert(win, Decoder::new(width, height));
                    first_win.get_or_insert(win);
                }
                Message::WinResize { win, width, height } => {
                    println!("+{:.2}s WIN_RESIZE {win} {width}x{height}", t0.elapsed().as_secs_f32());
                    if let Some(d) = decoders.get_mut(&win) {
                        d.reset(width, height);
                    }
                    resized_at = Some(Instant::now());
                }
                Message::WinTitle { win, title } => println!("WIN_TITLE {win} {title:?}"),
                Message::WinDestroy { win } => {
                    println!("+{:.2}s WIN_DESTROY {win}", t0.elapsed().as_secs_f32());
                    decoders.remove(&win);
                    closed_ok = true;
                }
                Message::Frame { win, seq, width, height, data, .. } => {
                    frames += 1;
                    bytes += data.len();
                    if let Some(d) = decoders.get_mut(&win) {
                        if (d.width(), d.height()) == (width, height) {
                            let _ = d.decode(&data);
                        }
                    }
                    if let Some(t) = resized_at.take() {
                        println!("+{:.2}s primeiro FRAME depois do WIN_RESIZE: {width}x{height}, {} B ({:.0} ms depois)", t0.elapsed().as_secs_f32(), data.len(), t.elapsed().as_secs_f32() * 1000.0);
                    }
                    write_message(&mut writer, &Message::FrameAck { win, seq }).unwrap();
                }
                Message::Ping { value } => write_message(&mut writer, &Message::Pong { echo: value }).unwrap(),
                _ => {}
            },
            Ok(Err(e)) => {
                println!("conexão encerrada: {e}");
                break;
            }
            Err(_) => {}
        }
        if let (Some(win), Some((w, h)), false) = (first_win, resize, sent_resize) {
            if t0.elapsed() > Duration::from_millis(1500) {
                println!("+{:.2}s RESIZE_REQ {win} {w}x{h}", t0.elapsed().as_secs_f32());
                write_message(&mut writer, &Message::ResizeReq { win, width: w, height: h }).unwrap();
                sent_resize = true;
            }
        }
        if let (Some(win), true, false) = (first_win, close, sent_close) {
            if t0.elapsed() > Duration::from_millis(if resize.is_some() { 3500 } else { 1500 }) {
                println!("+{:.2}s CLOSE_REQ {win}", t0.elapsed().as_secs_f32());
                write_message(&mut writer, &Message::CloseReq { win }).unwrap();
                sent_close = true;
            }
        }
        if last.elapsed() >= Duration::from_secs(1) {
            println!("+{:.0}s: {frames} quadros, {:.1} KiB ({:.1} KiB/s)", t0.elapsed().as_secs_f32(), bytes as f64 / 1024.0, bytes as f64 / 1024.0 / last.elapsed().as_secs_f64());
            frames = 0;
            bytes = 0;
            last = Instant::now();
        }
    }
    if close {
        println!("fechamento {}", if closed_ok { "confirmado (WIN_DESTROY recebido)" } else { "NÃO confirmado" });
    }
}
