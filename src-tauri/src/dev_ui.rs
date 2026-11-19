//! 调试构建的窗口地址是**编译期**注入的 devUrl——Tauri CLI 在 `tauri dev` 下会起一个
//! 内置静态服务（默认 `http://127.0.0.1:1430`）并把 `frontendDist` 指过去。
//! 但从开始菜单/桌面快捷方式直接开 debug exe 时，那个服务并不存在，
//! 窗口会整屏 `ERR_CONNECTION_REFUSED`（本项目真实踩过：快捷方式指向的就是 debug exe，
//! 而 `ui/` 是纯静态文件、没有构建步骤，exe 自己不会服务它们）。
//!
//! 这里在 `main()` 里先探一下那个端口：
//! - 有人听 → 让位（`tauri dev` 正在跑，用它的热重载服务）；
//! - 没人听 → 自己用 tiny_http 起一个同样的静态服务，把 `ui/` 服出去。
//!
//! 只在 dev 构建里编译（`cfg(dev)`），release 完全不受影响。
//!
//! 注意端口：兜底服务会一直占着 devUrl 的端口。所以「用快捷方式开了驾驶舱」之后
//! 再跑 `npm run tauri dev`，那边的内置服务会因端口被占而起不来——
//! 退出驾驶舱再跑即可（届时窗口又会走回 tauri 的热重载服务）。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tauri::utils::config::Config;
use tiny_http::{Header, Response, Server};

/// 探一下编译期注入的 devUrl：端口空着就起个静态服务兜底。
pub fn ensure(cfg: &Config) {
    let url = match cfg.build.dev_url.as_ref() {
        Some(u) => u.as_str(),
        // 生产构建没有 devUrl（窗口走内嵌资源的 tauri:// 协议），不用管
        None => return,
    };
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => return,
    };
    let host = rest.split('/').next().unwrap_or("");
    let addr: SocketAddr = match host.parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    // 已经有人在服（tauri dev 的内置服务，或别的程序占了端口）——让位，不抢
    if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
        return;
    }
    // ui/ 是纯静态目录，位置跟着仓库走（本 crate 的 manifest 在 src-tauri/）
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui");
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => return,
    };
    let server = match Server::http(addr) {
        Ok(s) => s,
        Err(_) => return,
    };
    std::thread::spawn(move || serve(server, root));
    // 给监听留个起步时间，窗口紧接着就要加载
    std::thread::sleep(Duration::from_millis(150));
}

fn serve(server: Server, root: PathBuf) {
    for req in server.incoming_requests() {
        let rel = req.url().split('?').next().unwrap_or("/");
        let mut path = root.join(rel.trim_start_matches('/'));
        if path.is_dir() {
            path = path.join("index.html");
        }
        let (code, ctype, body) = match std::fs::read(&path) {
            Ok(b) => (200, content_type(&path), b),
            Err(_) => (404, "text/plain; charset=utf-8", b"404 Not Found".to_vec()),
        };
        let mut resp = Response::from_data(body).with_status_code(code);
        if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()) {
            resp.add_header(h);
        }
        let _ = req.respond(resp);
    }
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::content_type;
    use std::path::Path;

    #[test]
    fn ui_suffixes_get_browser_usable_types() {
        assert_eq!(content_type(Path::new("index.html")), "text/html; charset=utf-8");
        assert_eq!(content_type(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(content_type(Path::new("styles.css")), "text/css; charset=utf-8");
        assert_eq!(content_type(Path::new("brand.png")), "image/png");
        // 后端一律兜到 octet-stream，不能让浏览器当 html 执行
        assert_eq!(content_type(Path::new("x.unknown")), "application/octet-stream");
    }
}
