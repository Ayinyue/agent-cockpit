# -*- coding: utf-8 -*-
"""T15 切生产前置：真实 vault.json 只读双通道比对。
PowerShell DPAPI（现行生产通道）vs 原生 CryptUnprotectData（ctypes 直调，与
dpapi_native.rs 同参数：CurrentUser + CRYPTPROTECT_UI_FORBIDDEN）。
只比 sha256 与长度，绝不打印明文/密钥。vault 文件不写不动。"""
import base64
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import pathlib
import subprocess
import sys

VAULT = pathlib.Path.home() / '.agent-cockpit' / 'vault.json'


class BLOB(ctypes.Structure):
    _fields_ = [('cbData', wt.DWORD), ('pbData', ctypes.POINTER(ctypes.c_char))]


def native_unprotect(data: bytes, entropy: bytes = b'') -> bytes:
    crypt32 = ctypes.windll.crypt32
    kernel32 = ctypes.windll.kernel32
    din = BLOB(len(data), ctypes.create_string_buffer(data, len(data)))
    ent = BLOB(len(entropy), ctypes.create_string_buffer(entropy, len(entropy))) if entropy else None
    out = BLOB()
    ok = crypt32.CryptUnprotectData(
        ctypes.byref(din), None,
        ctypes.byref(ent) if ent else None,
        None, None, 0x01, ctypes.byref(out))  # CRYPTPROTECT_UI_FORBIDDEN
    if not ok:
        raise OSError(f'CryptUnprotectData failed: {ctypes.GetLastError()}')
    try:
        return ctypes.string_at(out.pbData, out.cbData)
    finally:
        kernel32.LocalFree(out.pbData)


def ps_unprotect(blob_b64: str) -> bytes:
    script = (
        "$ErrorActionPreference='Stop';Add-Type -AssemblyName System.Security;"
        "$in=[Convert]::FromBase64String($env:COCKPIT_IN);"
        "$out=[Security.Cryptography.ProtectedData]::Unprotect($in,$null,"
        "[Security.Cryptography.DataProtectionScope]::CurrentUser);"
        "[Console]::Out.Write([Convert]::ToBase64String($out))")
    enc = base64.b64encode(script.encode('utf-16-le')).decode('ascii')
    import os
    env = dict(os.environ, COCKPIT_IN=blob_b64)
    r = subprocess.run(['powershell', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
                        '-EncodedCommand', enc], capture_output=True, env=env, timeout=60,
                       creationflags=0x08000000)
    if r.returncode != 0:
        raise OSError(r.stderr.decode('utf-8', 'replace')[-200:])
    return base64.b64decode(r.stdout.decode('ascii').strip())


def main():
    m = json.loads(VAULT.read_text(encoding='utf-8'))
    if m.get('has_password'):
        print('vault 设了主密码：本探针只覆盖无 entropy 路径，跳过（需人工解锁后验证）')
        return 2
    blob = base64.b64decode(m['blob'])
    a = ps_unprotect(m['blob'])
    b = native_unprotect(blob)
    ha, hb = hashlib.sha256(a).hexdigest(), hashlib.sha256(b).hexdigest()
    print('PS     len:', len(a), 'sha256:', ha[:16], '…')
    print('native len:', len(b), 'sha256:', hb[:16], '…')
    assert a == b, '双通道明文不一致——禁止切换！'
    # 明文应是 JSON object（secrets.rs flush 的 serde_json map）；只验证结构与条数
    obj = json.loads(a)
    assert isinstance(obj, dict)
    print('明文结构: JSON object, keys =', len(obj), '| vault.count =', m.get('count'))
    assert len(obj) == m.get('count'), '条数与元数据不符'
    # 反向：原生加密 → PS 解（写路径互操作，合成数据）
    probe = b't15-write-path-probe-synthetic'
    crypt32 = ctypes.windll.crypt32
    din = BLOB(len(probe), ctypes.create_string_buffer(probe, len(probe)))
    out = BLOB()
    assert crypt32.CryptProtectData(ctypes.byref(din), None, None, None, None, 0x01, ctypes.byref(out))
    try:
        nb = ctypes.string_at(out.pbData, out.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(out.pbData)
    back = ps_unprotect(base64.b64encode(nb).decode('ascii'))
    assert back == probe, '原生加密 → PS 解密不一致'
    print('写路径互操作: 原生 Protect → PS Unprotect 一致')
    print('REAL VAULT DUAL-CHANNEL OK')
    return 0


if __name__ == '__main__':
    sys.exit(main())