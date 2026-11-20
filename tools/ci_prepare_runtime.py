"""CI helper: materialize the vendored Python runtime from tracked vendor zips.

Golden 驱动脚本要求内置 python（~/.agent-cockpit/usage-runtime/python-3.14.10-*/python.exe，
带 cryptography）。CI 不 pip 安装任何东西：直接解出与产品运行时逐字节一致的 vendored 运行时
（python-embed.zip 全量 + agenthub-libs.zip 覆盖 _pth 加 site-packages，与 usage.rs
prepare_runtime 同序）。stdlib 的 compression.zstd（3.14 起）供 chatgpt-history 的 .zst 用例。

用法：python tools/ci_prepare_runtime.py [dest]
缺省 dest = ~/.agent-cockpit/usage-runtime/python-3.14.10-ci（与产品哈希命名目录区分开；
golden 驱动按 glob 排序取第一个，本机已有真实运行时时不受影响）。
自检失败即非零退出（fail closed），绝不静默降级到系统 python。
"""
import pathlib
import subprocess
import sys
import zipfile


def main():
    root = pathlib.Path(__file__).resolve().parents[1]
    if len(sys.argv) > 1:
        rt = pathlib.Path(sys.argv[1]).resolve()
    else:
        rt = pathlib.Path.home() / ".agent-cockpit" / "usage-runtime" / "python-3.14.10-ci"
    rt.mkdir(parents=True, exist_ok=True)
    # 顺序与 usage.rs 一致：先解释器，后 libs（libs 的 _pth 覆盖解释器的 _pth）。
    # 两个 zip 都是 git 跟踪的产品发布物，信任级别与仓库源码相同。
    for name in ("python-embed.zip", "agenthub-libs.zip"):
        with zipfile.ZipFile(root / "src-tauri" / "vendor" / name) as archive:
            archive.extractall(rt)
    pth = (rt / "python314._pth").read_text(encoding="utf-8")
    if "site-packages" not in pth:
        raise SystemExit("agenthub-libs _pth must add Lib\\site-packages")
    py = rt / "python.exe"
    # 不用 -I：隔离由 _pth 天然提供（embeddable 发行版不读 PYTHONPATH/注册表），
    # -I 会触发本机沙箱的删除审计拦截（CHAT_REFERENCE_PROTECTED），CI 与本地两栖。
    check = subprocess.run(
        [str(py), "-B", "-X", "utf8", "-c",
         "import cryptography, compression.zstd, sqlite3; "
         "print('vendored deps OK', cryptography.__version__)"],
        capture_output=True, text=True, timeout=120,
    )
    sys.stdout.write(check.stdout)
    sys.stderr.write(check.stderr)
    if check.returncode != 0:
        raise SystemExit(check.returncode)
    print("vendored runtime ready:", rt)


if __name__ == "__main__":
    main()