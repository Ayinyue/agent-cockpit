"""CI helper: privacy gate over git-tracked files only.

privacy_check.py 默认扫「跟踪 + 未跟踪（--others）」文件；CI runner 上 registry 缓存、
解出的 vendored 运行时、ci-target 编译产物都是未跟踪文件，第三方 crate 里的示例邮箱/
合成密钥会被误报成 findings。本 helper 只喂 git ls-files 清单（发布口径：入库什么扫什么），
规则引擎复用 privacy_check 本体，不另立口径。二进制扫描仍走 privacy_check --binary。

用法：python tools/ci_privacy_gate.py   （工作目录 = 仓库根；有 findings 即非零退出）
"""
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import privacy_check as pc  # noqa: E402


def main():
    root = pathlib.Path.cwd()
    names = subprocess.check_output(["git", "ls-files", "-z"], cwd=root).decode().split("\0")
    terms = pc.private_terms()
    findings = []
    for name in names:
        if not name:
            continue
        path = root / name
        if not path.is_file():
            continue
        if name == "tools/privacy_check.py":
            continue  # rule definitions are not product data（与 privacy_check.scan 同口径）
        try:
            text = path.read_text(encoding="utf-8-sig")
        except (UnicodeError, OSError):
            findings.extend(pc.scan_binary(path, terms))
            continue
        findings.extend(pc.scan_text(text, path.relative_to(root).as_posix(), terms))
    for name, line, category in findings:
        print(f"{name}:{line}: {category}")
    print(f"Privacy gate (tracked source): {len(findings)} findings; values withheld.")
    return bool(findings)


if __name__ == "__main__":
    raise SystemExit(main())