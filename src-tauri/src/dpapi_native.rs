// T15（阶段四地基）：原生 Windows DPAPI（CryptProtectData/CryptUnprotectData），
// 为把 secrets.rs 的「起 PowerShell 做 DPAPI」迁到进程内原生调用铺路。
//
// **本轮只建原语 + golden 测试，不接生产保管箱。** 切换生产 vault.json 加解密属数据丢失级
// 风险（计划红线 §7：熵/scope 必须与现状一致，否则旧保管箱解不开），须真实样本 + 单独授权。
// 这里用 CurrentUser scope + CRYPTPROTECT_UI_FORBIDDEN（与 secrets.rs 的 PowerShell 通道同口径），
// golden 测试验证「原生 DPAPI 与 PowerShell DPAPI 互解一致」，供后续迁移复用。
//
// 仅 Windows 编译；其它平台本模块为空（secrets.rs 本就只在 Windows 用 DPAPI）。

#![cfg(windows)]

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
};

/// 把一段字节交给 DPAPI 加密（CurrentUser scope，可选 entropy）。
/// 与 secrets.rs PowerShell 通道的 `[Security.Cryptography.ProtectedData]::Protect(..., CurrentUser)` 同口径。
pub fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let ent = if entropy.is_empty() {
            None
        } else {
            Some(&CRYPT_INTEGER_BLOB {
                cbData: entropy.len() as u32,
                pbData: entropy.as_ptr() as *mut u8,
            } as *const CRYPT_INTEGER_BLOB)
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(
            &data_in,
            windows::core::PCWSTR::null(),
            ent,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("DPAPI 加密失败：{e}"))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        Ok(bytes)
    }
}

/// 解开一段 DPAPI 密文（CurrentUser scope，须用与加密时相同的 entropy）。
pub fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let ent = if entropy.is_empty() {
            None
        } else {
            Some(&CRYPT_INTEGER_BLOB {
                cbData: entropy.len() as u32,
                pbData: entropy.as_ptr() as *mut u8,
            } as *const CRYPT_INTEGER_BLOB)
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(
            &data_in,
            None,
            ent,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("DPAPI 解密失败：{e}"))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 原生 DPAPI 回环（合成数据，非真实凭据）。
    #[test]
    fn dpapi_native_roundtrip() {
        let plain = b"synthetic-dpapi-payload-T15";
        let blob = protect(plain, &[]).expect("加密应成功");
        assert_ne!(blob, plain, "密文不应等于明文");
        let back = unprotect(&blob, &[]).expect("解密应成功");
        assert_eq!(back, plain);
    }

    /// 带 entropy：必须用同一段才解得开（与 secrets.rs 主密码 entropy 同语义）。
    #[test]
    fn dpapi_native_entropy_sensitive() {
        let plain = b"synthetic-with-entropy";
        let ent = b"entropy-from-password";
        let blob = protect(plain, ent).unwrap();
        assert!(unprotect(&blob, b"wrong-entropy").is_err(), "错 entropy 必须解不开");
        assert_eq!(unprotect(&blob, ent).unwrap(), plain);
    }

    /// T15 golden：原生 DPAPI 加密 → PowerShell DPAPI 解密一致（跨实现互操作）。
    /// 由 tests/dpapi-native-golden.test.py 触发：它读环境变量里的密文，用 PowerShell 解，比对。
    /// 这里只负责「原生加密」一侧；未设环境变量时跳过。
    #[test]
    fn dpapi_native_golden_emit() {
        let Ok(path) = std::env::var("DPAPI_NATIVE_GOLDEN_EMIT") else {
            eprintln!("跳过 golden：未设 DPAPI_NATIVE_GOLDEN_EMIT（由 tests/dpapi-native-golden.test.py 触发）");
            return;
        };
        // 合成明文 + entropy，原生加密后把 (plain_b64, entropy_b64, blob_b64) 写文件供 PowerShell 解
        let plain = b"golden-synthetic-dpapi-T15";
        let entropy = b"golden-entropy";
        let blob = protect(plain, entropy).unwrap();
        let v = serde_json::json!({
            "plain_b64": crate::secrets::b64_encode(plain),
            "entropy_b64": crate::secrets::b64_encode(entropy),
            "blob_b64": crate::secrets::b64_encode(&blob),
        });
        std::fs::write(&path, v.to_string()).expect("应能写 golden 向量");
    }
}