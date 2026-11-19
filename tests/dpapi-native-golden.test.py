"""T15 golden：原生 DPAPI（Rust CryptProtectData）与 PowerShell DPAPI 跨实现互操作验证。

方向：Rust 原生加密 → PowerShell [Security.Cryptography.ProtectedData]::Unprotect 解密，
比对明文逐字节一致。证明原生 DPAPI 与 secrets.rs 现用的 PowerShell 通道同口径，
后续把保管箱迁到原生调用时旧密文仍可解（CurrentUser scope + 同 entropy）。

全部合成数据，绝不读真实 vault.json / 登录态。本脚本不接生产保管箱。
"""
import base64, json, os, pathlib, subprocess, sys, tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path.home() / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"

vf = pathlib.Path(tempfile.gettempdir()) / "dpapi-native-golden.json"
env = dict(os.environ)
env["DPAPI_NATIVE_GOLDEN_EMIT"] = str(vf)
env["CARGO_TARGET_DIR"] = str(TARGET)

# 1) Rust 原生加密，写向量
p = subprocess.run(
    [str(CARGO), "test", "--bin", "agent-cockpit", "dpapi_native_golden_emit", "--", "--nocapture"],
    cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=600,
)
if p.returncode != 0:
    print("Rust 原生加密失败:", (p.stdout + p.stderr).decode("utf-8", "replace")[-1500:]); sys.exit(1)
if not vf.is_file():
    print("未生成向量文件"); sys.exit(1)
v = json.loads(vf.read_text(encoding="utf-8"))

# 2) PowerShell 用同 entropy 解密，比对明文
ps = r'''
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Security
$blob=[Convert]::FromBase64String($env:G_BLOB)
$ent=[Convert]::FromBase64String($env:G_ENT)
$out=[Security.Cryptography.ProtectedData]::Unprotect($blob,$ent,[Security.Cryptography.DataProtectionScope]::CurrentUser)
[Console]::Out.Write([Convert]::ToBase64String($out))
'''
env2 = dict(os.environ)
env2["G_BLOB"] = v["blob_b64"]
env2["G_ENT"] = v["entropy_b64"]
q = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", ps],
                   capture_output=True, timeout=120, env=env2)
if q.returncode != 0:
    print("PowerShell 解密失败:", q.stderr.decode("utf-8", "replace")[:500]); sys.exit(1)
ps_plain_b64 = q.stdout.decode("utf-8").strip()
ok = ps_plain_b64 == v["plain_b64"]
print(json.dumps({
    "native_encrypt_ps_decrypt_match": ok,
    "plain_b64": v["plain_b64"],
    "ps_decrypted_b64": ps_plain_b64,
}, ensure_ascii=False))
sys.exit(0 if ok else 1)