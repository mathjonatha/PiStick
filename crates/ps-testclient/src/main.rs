//! `ps-testclient`: cliente Wayland de teste do compositor do `psd`.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use ps_testclient::{run, Options};

    let mut opts = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--log" => opts.log = args.next().map(Into::into),
            "--title" => opts.title = args.next().unwrap_or_default(),
            "--no-poison" => opts.poison = false,
            "--popups" => opts.popups = true,
            "--kde-decoration" => opts.kde_decoration = true,
            "--clipboard" => opts.clipboard = args.next(),
            "--cursor" => {
                opts.cursor = match args.next().as_deref() {
                    Some("image") => Some(ps_testclient::CursorMode::Image),
                    Some("image-late") => Some(ps_testclient::CursorMode::ImageLate),
                    Some("hidden") => Some(ps_testclient::CursorMode::Hidden),
                    Some(nome) => Some(ps_testclient::CursorMode::Named(nome.to_string())),
                    None => {
                        eprintln!("ps-testclient: --cursor precisa de um nome, image, image-late ou hidden");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            "--margin" => opts.margin = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--min-size" | "--max-size" | "--self-resize" => {
                let tam = args.next().and_then(|v| {
                    let (l, a) = v.split_once('x')?;
                    Some((l.parse::<i32>().ok()?, a.parse::<i32>().ok()?))
                });
                match (a.as_str(), tam) {
                    ("--min-size", Some(t)) => opts.min_size = Some(t),
                    ("--max-size", Some(t)) => opts.max_size = Some(t),
                    ("--self-resize", Some(t)) => opts.self_resize = Some((t.0 as u32, t.1 as u32)),
                    _ => {
                        eprintln!("ps-testclient: {a} precisa de LARGURAxALTURA");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            "--socket" => opts.socket = args.next().map(Into::into),
            "-h" | "--help" => {
                println!("ps-testclient [--log ARQUIVO] [--title TÍTULO] [--socket CAMINHO] [--no-poison]\n  [--popups (teclas p, c, t, u, v, b)] [--cursor NOME|image|image-late|hidden] [--clipboard TEXTO (tecla y)] [--margin PX] [--min-size LxA] [--max-size LxA] [--self-resize LxA (ao apertar a tecla r)]\nUsa WAYLAND_DISPLAY e XDG_RUNTIME_DIR se --socket não for dado.");
                return std::process::ExitCode::SUCCESS;
            }
            outro => {
                eprintln!("ps-testclient: argumento desconhecido: {outro}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    match run(opts) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ps-testclient: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ps-testclient só existe no Linux");
}
