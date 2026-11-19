//! 内置 agenthub（agent-memory-suite, MIT）的命令行入口。
//!
//! 发布版优先跑 `usage-runtime/` 里解压出来的 Python + agenthub.py，
//! 这样签到/额度不再依赖用户另装 agent-memory-suite。
//! 内置运行时未通过校验时将错误返回调用方。
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 默认超时：agenthub 要打官方接口，慢的时候要留足余量；但绝不能无限等。
/// 原先 checkin / local_quota / traeproxy / doubao 四处都是裸 `.output()`：
/// 上游挂住或 TLS 停等时，后台循环就永久卡在那一次调用上，后面的账号全排不上。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// 带超时地跑完一条 agenthub 命令，返回 (status_success, stdout, stderr)。
///
/// 纪律与 `usage.rs` 的 run_helper 一致：
/// 1. **边跑边抽 stdout/stderr**——先等退出再读会在输出超过管道缓冲时死锁。
/// 2. 超时就 kill + wait 收尸，不留僵尸进程，并把超时如实报成错误（不假装成功）。
/// 3. 调用方拿到的是明确 Err，可以据此显示错误、继续下一个账号。
pub fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.by_ref().take(64 * 1024 * 1024).read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let out_handle = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_handle = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!("内置后端超过 {} 秒未返回，已中止本次读取。", timeout.as_secs()));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(e.to_string());
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

/// 用默认超时跑一条命令。多数调用点用这个就够。
pub fn output_with_timeout(command: Command) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    run_with_timeout(command, DEFAULT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用内置 python 起一个可控的子进程：不依赖 agenthub 脚本，也不需要网络。
    fn python_command(code: &str) -> Command {
        let python = crate::usage::bundled_python().expect("内置 python 应可用");
        let mut c = Command::new(python);
        c.args(["-I", "-B", "-X", "utf8", "-c", code]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        c
    }

    #[test]
    fn timeout_kills_a_hung_backend_instead_of_blocking_forever() {
        // 故意卡死：睡 30 秒。给 1 秒超时，必须很快拿到 Err，且不许把 30 秒等完。
        // 计时**只覆盖 run_with_timeout**：构造命令要先解压校验 34.5MB 内置运行时，
        // 那几秒属于准备成本，算进来会把「超时是否生效」这条断言测成假的（实测 10.2s 误红）。
        let command = python_command("import time; time.sleep(30)");
        let started = Instant::now();
        let result = run_with_timeout(command, Duration::from_secs(1));
        let elapsed = started.elapsed();
        let error = result.expect_err("卡死的后端必须报超时错误，不能假装成功");
        assert!(error.contains("未返回"), "错误要说清是超时：{error}");
        assert!(elapsed < Duration::from_secs(8), "超时应及时生效，实际等了 {elapsed:?}");
    }

    #[test]
    fn large_output_is_drained_without_deadlock() {
        // 输出远超管道缓冲（通常 64KB）：先 wait 再读的实现会在这里死锁。
        // 这条断言钉住「边跑边抽」这个纪律。
        let (success, stdout, _stderr) = run_with_timeout(
            python_command("import sys; sys.stdout.write('x' * 2_000_000)"),
            Duration::from_secs(60),
        ).expect("正常命令应成功");
        assert!(success);
        assert_eq!(stdout.len(), 2_000_000, "大输出必须被完整抽干");
    }

    #[test]
    fn failure_keeps_stderr_and_reports_non_success() {
        let (success, _stdout, stderr) = run_with_timeout(
            python_command("import sys; sys.stderr.write('boom'); sys.exit(3)"),
            Duration::from_secs(60),
        ).expect("进程能跑完，只是退出码非 0");
        assert!(!success, "非 0 退出码必须如实报 false");
        assert_eq!(String::from_utf8_lossy(&stderr).trim(), "boom");
    }
}

pub fn bundled_available() -> bool {
    crate::usage::bundled_agenthub().is_ok()
}

/// AStudio 的资料目录跟着安装位置走，不在 ~ 下。驾驶舱已经从 exe 推出
/// `<安装目录上一级>/AStudio Data/userdata`，这里把它的上一级交给 agenthub，
/// 否则内置版按默认安装位置找不到本机装在别处的 AStudio。
fn apply_astudio_env(c: &mut Command) {
    if std::env::var_os("ASTUDIO_HOME").is_some() {
        return;
    }
    let Some(app) = crate::registry::app_def("astudio") else { return };
    let Some(exe) = crate::registry::resolve_exe(&app) else { return };
    let Some(data) = crate::registry::astudio_data_dir(&exe) else { return };
    if let Some(root) = data.parent() {
        c.env("ASTUDIO_HOME", root);
    }
}

pub fn command(args: &[&str]) -> Result<Command, String> {
    let (python, script) = crate::usage::bundled_agenthub()?;
    let mut c = Command::new(python);
    c.args(["-I", "-B", "-X", "utf8"]).arg(script).args(args);
    let base = crate::registry::base_dir();
    c.env("AGENTHUB_HOME", base.join("agenthub-state"));
    c.env("AGENT_COCKPIT_HOME", base);
    if let Some(secret) = crate::wbproxy::at_rest_secret() {
        c.env("AGENTHUB_WORKBUDDY_AT_REST_SECRET", secret);
    }
    apply_astudio_env(&mut c);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(c)
}
