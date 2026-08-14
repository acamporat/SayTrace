use crate::error::{CoreError, CoreResult};

const ENTROPY: &[u8] = b"Local Transcript voice embedding v1";

#[cfg(target_os = "macos")]
mod macos_keychain {
    use std::sync::{Mutex, OnceLock};

    use aes_gcm::{
        aead::{Aead, KeyInit, Payload},
        Aes256Gcm, Nonce,
    };
    use security_framework::passwords::{get_generic_password, set_generic_password};
    use zeroize::Zeroize;

    use super::{CoreError, CoreResult, ENTROPY};

    const SERVICE: &str = "com.localtranscript.desktop.voice-profiles";
    const ACCOUNT: &str = "embedding-key-v1";
    const PREFIX: &[u8; 5] = b"STKC1";
    const KEY_BYTES: usize = 32;
    const NONCE_BYTES: usize = 12;
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;
    static KEY: OnceLock<[u8; KEY_BYTES]> = OnceLock::new();
    static KEY_INITIALIZATION: Mutex<()> = Mutex::new(());

    fn key() -> CoreResult<&'static [u8; KEY_BYTES]> {
        if let Some(key) = KEY.get() {
            return Ok(key);
        }
        let _guard = KEY_INITIALIZATION.lock().map_err(|_| {
            CoreError::Security("Keychain key initialization lock was poisoned".into())
        })?;
        if let Some(key) = KEY.get() {
            return Ok(key);
        }
        let mut loaded = match get_generic_password(SERVICE, ACCOUNT) {
            Ok(value) => value,
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
                let mut generated = [0_u8; KEY_BYTES];
                getrandom::fill(&mut generated).map_err(|error| {
                    CoreError::Security(format!(
                        "could not generate a Keychain encryption key: {error}"
                    ))
                })?;
                if let Err(error) = set_generic_password(SERVICE, ACCOUNT, &generated) {
                    generated.zeroize();
                    return Err(CoreError::Security(format!(
                        "could not store the voice-profile key in Keychain: {error}"
                    )));
                }
                let loaded = generated.to_vec();
                generated.zeroize();
                loaded
            }
            Err(error) => {
                return Err(CoreError::Security(format!(
                    "could not read the voice-profile key from Keychain: {error}"
                )))
            }
        };
        if loaded.len() != KEY_BYTES {
            loaded.zeroize();
            return Err(CoreError::Security(
                "the Keychain voice-profile key has an invalid length".into(),
            ));
        }
        let mut material = [0_u8; KEY_BYTES];
        material.copy_from_slice(&loaded);
        loaded.zeroize();
        let _ = KEY.set(material);
        KEY.get().ok_or_else(|| {
            CoreError::Security("could not initialize the Keychain voice-profile key".into())
        })
    }

    pub fn protect(bytes: &[u8]) -> CoreResult<Vec<u8>> {
        if bytes.is_empty() {
            return Err(CoreError::InvalidInput(
                "voice embedding must not be empty".into(),
            ));
        }
        let cipher = Aes256Gcm::new_from_slice(key()?).map_err(|_| {
            CoreError::Security("could not initialize voice-profile encryption".into())
        })?;
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        getrandom::fill(&mut nonce_bytes).map_err(|error| {
            CoreError::Security(format!("could not generate a voice-profile nonce: {error}"))
        })?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: bytes,
                    aad: ENTROPY,
                },
            )
            .map_err(|_| {
                CoreError::Security("Keychain-backed voice-profile encryption failed".into())
            })?;
        let mut output = Vec::with_capacity(PREFIX.len() + NONCE_BYTES + encrypted.len());
        output.extend_from_slice(PREFIX);
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&encrypted);
        Ok(output)
    }

    pub fn unprotect(bytes: &[u8]) -> CoreResult<Vec<u8>> {
        if bytes.len() <= PREFIX.len() + NONCE_BYTES || !bytes.starts_with(PREFIX) {
            return Err(CoreError::Security(
                "voice-profile data is not a supported Keychain-backed envelope".into(),
            ));
        }
        let nonce_start = PREFIX.len();
        let ciphertext_start = nonce_start + NONCE_BYTES;
        let cipher = Aes256Gcm::new_from_slice(key()?).map_err(|_| {
            CoreError::Security("could not initialize voice-profile decryption".into())
        })?;
        cipher
            .decrypt(
                Nonce::from_slice(&bytes[nonce_start..ciphertext_start]),
                Payload {
                    msg: &bytes[ciphertext_start..],
                    aad: ENTROPY,
                },
            )
            .map_err(|_| {
                CoreError::Security("Keychain-backed voice-profile decryption failed".into())
            })
    }
}

#[cfg(windows)]
mod windows_dpapi {
    use std::{ffi::c_void, ptr, slice};

    use super::{CoreError, CoreResult, ENTROPY};
    use zeroize::Zeroize;

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            optional_entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt_struct: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;

        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description: *mut *mut u16,
            optional_entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt_struct: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    pub fn protect(bytes: &[u8]) -> CoreResult<Vec<u8>> {
        crypt(bytes, true)
    }

    pub fn unprotect(bytes: &[u8]) -> CoreResult<Vec<u8>> {
        crypt(bytes, false)
    }

    fn crypt(bytes: &[u8], encrypt: bool) -> CoreResult<Vec<u8>> {
        if bytes.is_empty() {
            return Err(CoreError::InvalidInput(
                "voice embedding must not be empty".into(),
            ));
        }
        let mut input_bytes = bytes.to_vec();
        let mut entropy_bytes = ENTROPY.to_vec();
        let input = DataBlob {
            cb_data: input_bytes.len().try_into().map_err(|_| {
                CoreError::InvalidInput("voice embedding exceeds DPAPI limits".into())
            })?,
            pb_data: input_bytes.as_mut_ptr(),
        };
        let entropy = DataBlob {
            cb_data: entropy_bytes.len() as u32,
            pb_data: entropy_bytes.as_mut_ptr(),
        };
        let mut output = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };
        let succeeded = unsafe {
            if encrypt {
                CryptProtectData(
                    &input,
                    ptr::null(),
                    &entropy,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    ptr::null_mut(),
                    &entropy,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if succeeded == 0 || output.pb_data.is_null() {
            return Err(CoreError::Security(
                "Windows DPAPI could not process the voice embedding".into(),
            ));
        }
        let protected = unsafe {
            let bytes = slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
            let _ = LocalFree(output.pb_data.cast());
            bytes
        };
        input_bytes.zeroize();
        entropy_bytes.zeroize();
        Ok(protected)
    }
}

pub fn protect_embedding(bytes: &[u8]) -> CoreResult<Vec<u8>> {
    #[cfg(windows)]
    {
        windows_dpapi::protect(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        macos_keychain::protect(bytes)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = bytes;
        Err(CoreError::Security(
            "voice profiles require Windows DPAPI".into(),
        ))
    }
}

pub fn unprotect_embedding(bytes: &[u8]) -> CoreResult<Vec<u8>> {
    #[cfg(windows)]
    {
        windows_dpapi::unprotect(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        macos_keychain::unprotect(bytes)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = bytes;
        Err(CoreError::Security(
            "voice profiles require Windows DPAPI".into(),
        ))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive login Keychain and creates the app encryption key"]
    fn keychain_backed_round_trip_is_authenticated() {
        let source = b"speaker-vector-binary";
        let protected = protect_embedding(source).unwrap();
        assert_ne!(protected, source);
        assert_eq!(unprotect_embedding(&protected).unwrap(), source);

        let mut tampered = protected;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(unprotect_embedding(&tampered).is_err());
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip_uses_current_user_scope() {
        let source = b"speaker-vector-binary";
        let protected = protect_embedding(source).unwrap();
        assert_ne!(protected, source);
        assert_eq!(unprotect_embedding(&protected).unwrap(), source);
    }
}
