// T14（阶段四地基）：自研加密原语，为把 agenthub 的 cryptography 解密迁到 Rust 铺路。
//
// **本轮只建原语 + golden 测试，不接任何生产加解密路径。** 切换生产 DPAPI / 登录态解密
// 属数据丢失级风险（计划红线 §7），须真实登录态样本 + 单独授权（T15/T16）。这里提供的
// 函数与 python cryptography 的输出逐字节一致（golden 测试用合成向量验证），供后续迁移复用。
//
// 覆盖 agenthub 实际用到的三类原语：
//   - AES-256-GCM 解密（Chromium os_crypt v10：blob = "v10" + 12B nonce + 密文+16B tag，aad=None）
//   - AES-256-CBC 解密（部分客户端登录态，PKCS7 填充）
//   - PBKDF2-HMAC-SHA512 派生（主密码 entropy，与 secrets.rs 的 ROUNDS 一致）

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use aes::Aes256;
use cbc::Decryptor;
use cipher::{BlockDecryptMut, KeyIvInit};
use serde_json::Value;

/// AES-256-GCM 解密。nonce 12 字节，密文含末尾 16 字节 tag，aad 可选。
/// 与 python `AESGCM(key).decrypt(nonce, data, aad)` 逐字节一致。
pub fn aes_gcm_decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if nonce.len() != 12 {
        return Err("GCM nonce 必须是 12 字节".into());
    }
    let cipher = Aes256Gcm::new(key.into());
    let n = Nonce::from_slice(nonce);
    let payload = aes_gcm::aead::Payload { msg: ciphertext, aad };
    cipher.decrypt(n, payload).map_err(|_| "AES-GCM 解密失败（密钥/nonce/tag 不符）".into())
}

/// Chromium os_crypt v10 整包解密：blob = b"v10" + 12B nonce + 密文+tag。
/// 与 agenthub.py 的 `AESGCM(aes).decrypt(blob[3:15], blob[15:], None)` 一致。
pub fn os_crypt_v10_decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 3 + 12 + 16 || &blob[..3] != b"v10" {
        return Err("不是有效的 os_crypt v10 包".into());
    }
    aes_gcm_decrypt(key, &blob[3..15], &blob[15..], &[])
}

/// AES-256-CBC 解密 + 去 PKCS7 填充。iv 16 字节。
/// 与 python `Cipher(AES(key), CBC(iv)).decryptor()` + PKCS7 unpadder 一致。
pub fn aes_cbc_decrypt(key: &[u8; 32], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    if iv.len() != 16 {
        return Err("CBC iv 必须是 16 字节".into());
    }
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err("CBC 密文长度必须是 16 的非零倍数".into());
    }
    let dec = Decryptor::<Aes256>::new(key.into(), iv.into());
    let mut buf = ciphertext.to_vec();
    let pt = dec
        .decrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf)
        .map_err(|_| String::from("AES-CBC 解密/去填充失败"))?;
    Ok(pt.to_vec())
}

/// PBKDF2-HMAC-SHA512 派生。与 python `PBKDF2HMAC(SHA512, length, salt, iterations)` 一致，
/// 也与 secrets.rs 主密码 entropy 派生（ROUNDS=150000）同参数。out 长度任意。
pub fn pbkdf2_sha512(password: &[u8], salt: &[u8], rounds: u32, out_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_len];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, rounds, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 合成密钥/向量（非真实凭据）。golden 比对由外部 python 脚本生成向量后在此断言，
    // 见 tests/agentcrypt-golden.test.py（python cryptography 加密 → 本模块解密逐字节一致）。

    /// GCM 基本回环：用固定合成密钥/nonce 加密再解密（加密仅供测试构造向量）。
    #[test]
    fn gcm_roundtrip_synthetic() {
        use aes_gcm::aead::Aead;
        let key = [0x42u8; 32];
        let nonce = [0x24u8; 12];
        let plaintext = b"synthetic-payload-for-t14";
        let cipher = Aes256Gcm::new((&key).into());
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), plaintext.as_slice()).unwrap();
        let pt = aes_gcm_decrypt(&key, &nonce, &ct, &[]).unwrap();
        assert_eq!(pt, plaintext);
    }

    /// os_crypt v10 整包：构造 "v10"+nonce+密文，解密回原文。
    #[test]
    fn os_crypt_v10_roundtrip_synthetic() {
        use aes_gcm::aead::Aead;
        let key = [0x11u8; 32];
        let nonce = [0x33u8; 12];
        let plaintext = b"{\"token\":\"synthetic\"}";
        let cipher = Aes256Gcm::new((&key).into());
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), plaintext.as_slice()).unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        assert_eq!(os_crypt_v10_decrypt(&key, &blob).unwrap(), plaintext);
        // 非 v10 前缀必须拒绝
        assert!(os_crypt_v10_decrypt(&key, b"v11xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").is_err());
    }

    /// CBC 回环（PKCS7）。
    #[test]
    fn cbc_roundtrip_synthetic() {
        use cbc::Encryptor;
        use cipher::BlockEncryptMut;
        let key = [0x55u8; 32];
        let iv = [0x66u8; 16];
        let plaintext = b"synthetic-cbc-payload";
        let enc = Encryptor::<Aes256>::new((&key).into(), (&iv).into());
        // 手动 PKCS7 填充到 16 倍数，再加密（不依赖 alloc feature）
        let pad_len = 16 - (plaintext.len() % 16);
        let mut buf = plaintext.to_vec();
        buf.extend(std::iter::repeat(pad_len as u8).take(pad_len));
        let ct = enc.encrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf, plaintext.len())
            .unwrap().to_vec();
        assert_eq!(aes_cbc_decrypt(&key, &iv, &ct).unwrap(), plaintext);
    }

    /// PBKDF2 已知答案（RFC 风格合成向量，与 python 比对见 golden 脚本）。
    /// 这里用固定输入断言确定性 + 长度，跨语言一致性由 golden 脚本钉住。
    #[test]
    fn pbkdf2_deterministic_and_length() {
        let a = pbkdf2_sha512(b"password", b"salt", 1000, 32);
        let b = pbkdf2_sha512(b"password", b"salt", 1000, 32);
        assert_eq!(a, b, "同输入必须同输出");
        assert_eq!(a.len(), 32);
        let c = pbkdf2_sha512(b"password", b"salt", 1001, 32);
        assert_ne!(a, c, "轮数不同必须不同");
        let d = pbkdf2_sha512(b"password", b"salt", 1000, 64);
        assert_eq!(d.len(), 64);
        assert_eq!(&d[..32], &a[..], "PBKDF2 前缀应一致（同参数不同长度）");
    }

    /// T14 golden：读 python cryptography 生成的合成向量，Rust 解密/派生逐字节比对。
    /// 由 tests/agentcrypt-golden.test.py 生成向量并设 AGENTCRYPT_GOLDEN_VECTORS 触发。
    /// 未设环境变量时跳过（常规 cargo test 不依赖外部文件）。
    /// 变异探针：把任一原语实现改坏（如 GCM nonce 切分错），本条即红。
    #[test]
    fn agentcrypt_golden_vectors() {
        let Ok(path) = std::env::var("AGENTCRYPT_GOLDEN_VECTORS") else {
            eprintln!("跳过 golden：未设 AGENTCRYPT_GOLDEN_VECTORS（由 tests/agentcrypt-golden.test.py 触发）");
            return;
        };
        let text = std::fs::read_to_string(&path).expect("应能读向量文件");
        let v: Value = serde_json::from_str(&text).expect("向量应是合法 JSON");
        let b64 = |s: &str| crate::secrets::b64_decode(s).expect("base64 解码");
        let key_v = b64(v["key_b64"].as_str().unwrap());
        let key: [u8; 32] = key_v.try_into().expect("key 应 32 字节");
        let nonce = b64(v["nonce_b64"].as_str().unwrap());
        let iv = b64(v["iv_b64"].as_str().unwrap());
        let pt = b64(v["plaintext_b64"].as_str().unwrap());

        // GCM：python 加密 → Rust 解密 == 原文
        let gcm_ct = b64(v["gcm_ct_b64"].as_str().unwrap());
        assert_eq!(aes_gcm_decrypt(&key, &nonce, &gcm_ct, &[]).unwrap(), pt, "GCM 跨语言必须逐字节一致");
        // os_crypt v10 整包
        let v10 = b64(v["v10_blob_b64"].as_str().unwrap());
        assert_eq!(os_crypt_v10_decrypt(&key, &v10).unwrap(), pt, "os_crypt v10 跨语言必须一致");
        // CBC
        let cbc_ct = b64(v["cbc_ct_b64"].as_str().unwrap());
        assert_eq!(aes_cbc_decrypt(&key, &iv, &cbc_ct).unwrap(), pt, "CBC 跨语言必须逐字节一致");
        // PBKDF2：python 派生 → Rust 派生 == 同值
        for case in v["pbkdf2"].as_array().unwrap() {
            let pwd = b64(case["pwd_b64"].as_str().unwrap());
            let salt = b64(case["salt_b64"].as_str().unwrap());
            let rounds = case["rounds"].as_u64().unwrap() as u32;
            let expected = b64(case["out_b64"].as_str().unwrap());
            let got = pbkdf2_sha512(&pwd, &salt, rounds, expected.len());
            assert_eq!(got, expected, "PBKDF2-SHA512 跨语言必须逐字节一致（rounds={rounds}）");
        }
    }
}