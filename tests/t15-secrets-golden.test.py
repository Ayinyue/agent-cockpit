# -*- coding: utf-8 -*-
"""T15 切生产 golden：secrets.rs 生产四原语（protect/unprotect/derive/random_b64）
Rust 原生优先路径 vs PowerShell 旧通道互操作。
模式（环境变量 T15_SECRETS_REQ 指向 JSON 请求）：
  emit  —— Rust 侧：原生 protect(合成明文, entropy) + derive(合成密码, salt, 1000/150000)
           + random_b64 ×2，写向量文件；
  verify —— Rust 侧：解 PowerShell 加密的 blob（含 entropy）+ 解旧格式向量，比对明文。
python 驱动负责 PowerShell 侧加解密与逐字节比对。合成数据，不碰真实凭据。"""
import base64
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"


def ps(script: str, envs: dict) -> bytes:
    enc = base64.b64encode(script.encode('utf-16-le')).decode('ascii')
    env = dict(os.environ, **envs)
    r = subprocess.run(['powershell', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
                        '-EncodedCommand', enc], capture_output=True, env=env, timeout=120,
                       creationflags=0x08000000)
    if r.returncode != 0:
        raise OSError(r.stderr.decode('utf-8', 'replace')[-300:])
    return base64.b64decode(r.stdout.decode('ascii').strip())


PS_PROTECT = ("$ErrorActionPreference='Stop';Add-Type -AssemblyName System.Security;"
              "$in=[Convert]::FromBase64String($env:COCKPIT_IN);"
              "$ent=if($env:COCKPIT_ENTROPY){[Convert]::FromBase64String($env:COCKPIT_ENTROPY)}else{$null};"
              "$out=[Security.Cryptography.ProtectedData]::Protect($in,$ent,[Security.Cryptography.DataProtectionScope]::CurrentUser);"
              "[Console]::Out.Write([Convert]::ToBase64String($out))")
PS_UNPROTECT = PS_PROTECT.replace('::Protect(', '::Unprotect(')
PS_DERIVE = ("$ErrorActionPreference='Stop';"
             "$p=New-Object System.Security.Cryptography.Rfc2898DeriveBytes($env:COCKPIT_PWD,"
             "[Convert]::FromBase64String($env:COCKPIT_SALT),[int]$env:COCKPIT_ROUNDS,"
             "[System.Security.Cryptography.HashAlgorithmName]::SHA512);"
             "[Console]::Out.Write([Convert]::ToBase64String($p.GetBytes(32)))")


def run_rust(req_path):
    env = dict(os.environ)
    env['CARGO_TARGET_DIR'] = str(TARGET)
    env['T15_SECRETS_REQ'] = str(req_path)
    p = subprocess.run([str(CARGO), 'test', '--bin', 'agent-cockpit',
                        'secrets::tests::t15_golden_bridge', '--', '--nocapture', '--exact'],
                       cwd=str(ROOT / 'src-tauri'), env=env, capture_output=True, timeout=900)
    out = (p.stdout + p.stderr).decode('utf-8', 'replace')
    for line in out.splitlines():
        if 'test result' in line or 'panicked' in line:
            print(line[:400])
    if p.returncode != 0:
        print(out[-2500:])
    return p.returncode


def main():
    tmp = pathlib.Path(tempfile.mkdtemp(prefix='t15-secrets-golden-')).resolve()
    plain = 'synthetic-credential-中文-🔐-{"k":"v"}'.encode('utf-8')
    entropy = hashlib.sha256(b't15-golden-entropy').digest()
    salt = hashlib.sha256(b't15-golden-salt').digest()[:16]
    pwd = 't15-合成密码-hunter2'

    # ---- 方向 A：Rust 原生 protect/derive/random → PowerShell 验证 ----
    emit_out = tmp / 'emit.json'
    req = tmp / 'req_emit.json'
    req.write_text(json.dumps({'mode': 'emit', 'out': str(emit_out),
                               'plain_b64': base64.b64encode(plain).decode(),
                               'entropy_b64': base64.b64encode(entropy).decode(),
                               'pwd': pwd, 'salt_b64': base64.b64encode(salt).decode()}), encoding='utf-8')
    if run_rust(req) != 0:
        return 1
    v = json.loads(emit_out.read_text(encoding='utf-8'))
    # A1: 原生密文（带 entropy）→ PS 解
    back = ps(PS_UNPROTECT, {'COCKPIT_IN': v['blob_b64'], 'COCKPIT_ENTROPY': v['entropy_b64']})
    assert back == plain, 'A1 原生→PS（entropy）不一致'
    # A2: 原生密文（无 entropy）→ PS 解
    back2 = ps(PS_UNPROTECT, {'COCKPIT_IN': v['blob_noent_b64'], 'COCKPIT_ENTROPY': ''})
    assert back2 == plain, 'A2 原生→PS（无 entropy）不一致'
    # A3: derive 1000 轮与 150000 轮 vs .NET Rfc2898
    for rounds in ('1000', '150000'):
        ref = ps(PS_DERIVE, {'COCKPIT_PWD': pwd, 'COCKPIT_SALT': v['salt_b64'], 'COCKPIT_ROUNDS': rounds})
        assert base64.b64decode(v['derive_' + rounds]) == ref, f'A3 derive {rounds} 轮不一致'
    # A4: random 两次不同且 32 字节
    r1, r2 = base64.b64decode(v['random1']), base64.b64decode(v['random2'])
    assert len(r1) == len(r2) == 32 and r1 != r2, 'A4 random 长度/熵异常'
    print('A: Rust 原生 protect/derive(1k+150k)/random → PowerShell 验证一致')

    # ---- 方向 B：PowerShell protect → Rust 生产 unprotect（含错 entropy 拒绝） ----
    ps_blob = base64.b64encode(ps(PS_PROTECT, {'COCKPIT_IN': base64.b64encode(plain).decode(),
                                               'COCKPIT_ENTROPY': base64.b64encode(entropy).decode()})).decode()
    ps_blob_noent = base64.b64encode(ps(PS_PROTECT, {'COCKPIT_IN': base64.b64encode(plain).decode(),
                                                     'COCKPIT_ENTROPY': ''})).decode()
    verify_out = tmp / 'verify.json'
    req2 = tmp / 'req_verify.json'
    req2.write_text(json.dumps({'mode': 'verify', 'out': str(verify_out),
                                'blob_b64': ps_blob, 'blob_noent_b64': ps_blob_noent,
                                'entropy_b64': base64.b64encode(entropy).decode(),
                                'wrong_entropy_b64': base64.b64encode(b'x' * 32).decode(),
                                'expect_plain_b64': base64.b64encode(plain).decode()}), encoding='utf-8')
    if run_rust(req2) != 0:
        return 1
    w = json.loads(verify_out.read_text(encoding='utf-8'))
    assert w['ent_ok'] and w['noent_ok'] and w['wrong_rejected'], f'B: 生产 unprotect 校验失败 {w}'
    print('B: PowerShell 密文 → Rust 生产 unprotect 一致；错 entropy 正确拒绝')
    print('T15 SECRETS GOLDEN OK')
    return 0


if __name__ == '__main__':
    sys.exit(main())