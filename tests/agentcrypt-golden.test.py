"""T14 golden 测试向量生成器 + 跨语言一致性验证（合成向量，不碰真实凭据）。

用 python cryptography 加密已知明文，生成向量 JSON；Rust agentcrypt 模块的
golden 测试（agentcrypt_golden_vectors）读该文件解密，断言逐字节一致。
本脚本负责：生成向量 → 设环境变量 → 触发 cargo test 跑 Rust golden 断言。

口径：AES-256-GCM（os_crypt v10 切分）、AES-256-CBC（PKCS7）、PBKDF2-HMAC-SHA512。
全部用固定合成密钥/明文，绝不读取真实登录态。
"""
import base64, json, os, pathlib, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUNTIME = pathlib.Path.home() / ".agent-cockpit" / "usage-runtime"
# 找内置 python（带 cryptography）
pys = sorted(RUNTIME.glob("python-3.14.10-*/python.exe"))
if not pys:
    print("找不到内置 python 运行时"); sys.exit(2)
PY = str(pys[0])

# 用内置 python 的 cryptography 生成向量（子进程，避免本进程缺依赖）
gen_code = r'''
import base64, json, os
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives import padding
from cryptography.hazmat.primitives.kdf.pbkdf2 import PBKDF2HMAC
from cryptography.hazmat.primitives import hashes

key = bytes(range(32))            # 合成密钥 0x00..0x1f
nonce = bytes(range(12))          # 合成 nonce
iv = bytes(range(16))             # 合成 iv
pt = b"golden-synthetic-payload-T14"

# GCM（aad=None，与 os_crypt v10 一致）
ct = AESGCM(key).encrypt(nonce, pt, None)
v10 = b"v10" + nonce + ct

# CBC + PKCS7
padder = padding.PKCS7(128).padder()
padded = padder.update(pt) + padder.finalize()
enc = Cipher(algorithms.AES(key), modes.CBC(iv)).encryptor()
cbc_ct = enc.update(padded) + enc.finalize()

# PBKDF2-SHA512（多组参数）
def kdf(pwd, salt, rounds, n):
    return PBKDF2HMAC(algorithm=hashes.SHA512(), length=n, salt=salt, iterations=rounds).derive(pwd)

vectors = {
    "key_b64": base64.b64encode(key).decode(),
    "nonce_b64": base64.b64encode(nonce).decode(),
    "iv_b64": base64.b64encode(iv).decode(),
    "plaintext_b64": base64.b64encode(pt).decode(),
    "gcm_ct_b64": base64.b64encode(ct).decode(),
    "v10_blob_b64": base64.b64encode(v10).decode(),
    "cbc_ct_b64": base64.b64encode(cbc_ct).decode(),
    "pbkdf2": [
        {"pwd_b64": base64.b64encode(b"hunter2").decode(), "salt_b64": base64.b64encode(b"nacl").decode(),
         "rounds": 1000, "out_b64": base64.b64encode(kdf(b"hunter2", b"nacl", 1000, 32)).decode()},
        {"pwd_b64": base64.b64encode(b"correct horse").decode(), "salt_b64": base64.b64encode(b"pepper").decode(),
         "rounds": 150000, "out_b64": base64.b64encode(kdf(b"correct horse", b"pepper", 150000, 32)).decode()},
    ],
}
print(json.dumps(vectors))
'''
r = subprocess.run([PY, "-I", "-B", "-X", "utf8", "-c", gen_code], capture_output=True, timeout=120)
if r.returncode != 0:
    print("向量生成失败:", r.stderr.decode("utf-8", "replace")[:500]); sys.exit(1)
vectors = json.loads(r.stdout.decode("utf-8"))

# 写向量文件到临时路径（不进仓库）
import tempfile
vf = pathlib.Path(tempfile.gettempdir()) / "agentcrypt-golden-vectors.json"
vf.write_text(json.dumps(vectors), encoding="utf-8")
print("向量已生成:", vf)

# 触发 Rust golden 测试（读该文件）
env = dict(os.environ)
env["AGENTCRYPT_GOLDEN_VECTORS"] = str(vf)
env["CARGO_TARGET_DIR"] = str(ROOT.parent / "agent-cockpit-health-review" / "cargo-target")
cargo = pathlib.Path.home() / ".cargo" / "bin" / "cargo.exe"
proc = subprocess.run(
    [str(cargo), "test", "--bin", "agent-cockpit", "agentcrypt_golden_vectors", "--", "--nocapture"],
    cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=600,
)
out = proc.stdout.decode("utf-8", "replace") + proc.stderr.decode("utf-8", "replace")
print(out[-2500:])
sys.exit(proc.returncode)