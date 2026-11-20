# -*- coding: utf-8 -*-
"""T18 golden：zcode-auth 迁 Rust 跨语言一致性验证。
同一份合成 record（含 enc:v1: 加密字段），python run vs Rust run 输出逐字段比对。
加密用 python cryptography 生成合成向量，Rust 用 T14 agentcrypt 解密——验证跨语言密码学一致。
只比对数字与结构，不打印凭据正文。合成数据，不碰真实凭据。"""
import base64
import hashlib
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"

# 与 python zcode-auth.py key() 同口径的合成密钥（固定 secret，不碰真实环境）
SECRET = "t18-golden-synthetic-secret"
KEY = hashlib.sha256(SECRET.encode("utf-8")).digest()


def b64url(b):
    return base64.urlsafe_b64encode(b).decode().rstrip("=")


def encrypt(plaintext):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    iv = bytes(range(12))
    ct = AESGCM(KEY).encrypt(iv, plaintext.encode("utf-8"), None)
    data, tag = ct[:-16], ct[-16:]
    return "enc:v1:" + ".".join([b64url(iv), b64url(tag), b64url(data)])


def make_record(provider, uid):
    return {
        "oauth:active_provider": encrypt(provider),
        f"oauth:{provider}:access_token": encrypt("synthetic-access-token"),
        f"oauth:{provider}:user_info": encrypt(json.dumps({"user_id": uid, "name": "Example", "email": "e@example.com"})),
        "zcodejwttoken": "",
        "bot:private:credential": "keep-bot",
        "project-key-location:v1:example": "keep-api",
    }


def main():
    spec = importlib.util.spec_from_file_location("z", ROOT / "src-tauri" / "scripts" / "zcode-auth.py")
    py = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(py)
    # 强制 python 用合成密钥（不读真实环境）
    os.environ["ZCODE_CREDENTIAL_SECRET"] = SECRET

    cases = []
    rec = make_record("bigmodel", "u1")
    cases.append(("identity", {"mode": "identity", "record": rec}, py.run({"mode": "identity", "record": rec})))
    cases.append(("snapshot", {"mode": "snapshot", "record": rec}, py.run({"mode": "snapshot", "record": rec})))
    # merge：current=zai, target=bigmodel
    current = make_record("zai", "one")
    target = make_record("bigmodel", "two")
    cases.append(("merge", {"mode": "merge", "record": current, "target": target},
                  py.run({"mode": "merge", "record": current, "target": target})))
    # merge logout
    cases.append(("merge_logout", {"mode": "merge", "record": current, "provider": "zai"},
                  py.run({"mode": "merge", "record": current, "provider": "zai"})))

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="t18-zcode-golden-"))
    failures = 0
    for name, request, py_value in cases:
        req_path = tmp / f"req_{name}.json"
        py_path = tmp / f"py_{name}.json"
        req_path.write_text(json.dumps(request, ensure_ascii=False), encoding="utf-8")
        py_path.write_text(json.dumps(py_value, ensure_ascii=False), encoding="utf-8")
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = str(TARGET)
        env["ZCODE_CREDENTIAL_SECRET"] = SECRET
        env["T18_ZCODE_REQ"] = str(req_path)
        env["T18_ZCODE_PY"] = str(py_path)
        p = subprocess.run(
            [str(CARGO), "test", "--bin", "agent-cockpit", "zcode_auth::tests::golden_matches_python",
             "--", "--nocapture", "--exact"],
            cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=600,
        )
        out = (p.stdout + p.stderr).decode("utf-8", "replace")
        for line in out.splitlines():
            if "test result" in line or "panicked" in line or "不一致" in line or "FAILED" in line:
                print(line[:400])
        print(f"[{name}] cargo_exit={p.returncode}")
        failures += (p.returncode != 0)
    print("golden failures:", failures)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())