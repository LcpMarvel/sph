//! sph 入口：信号处理 + CLI 调度。

use std::io::{BufWriter, Read, Write};
use std::sync::{Arc, Mutex};

use sph::cli::run::{self, Deps, SharedWriter};
use sph::http::CancelToken;

fn main() {
    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(real_main()))
        .unwrap_or(1);
    std::process::exit(code);
}

async fn real_main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cancel = CancelToken::default();
    let deps = Deps {
        cancel: cancel.clone(),
        ..Deps::production()
    };

    // Ctrl+C：第一次优雅取消，第二次强制退出
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        let mut count = 0;
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            count += 1;
            if count == 1 {
                cancel_for_signal.cancel();
            } else {
                std::process::exit(130);
            }
        }
    });

    let stdin: Arc<Mutex<dyn Read + Send>> = Arc::new(Mutex::new(std::io::stdin()));
    let stdout_raw = std::io::stdout();
    let stderr_raw = std::io::stderr();
    let mut stdout = BufWriter::new(stdout_raw.lock());
    let _ = &stderr_raw;
    let _ = stderr_raw;
    let stderr: SharedWriter = Arc::new(Mutex::new(BufWriter::new(std::io::stderr())));

    let code = run::run(&argv, stdin, &mut stdout, stderr, &deps).await;
    let _ = stdout.flush();
    code
}
