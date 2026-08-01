use crate::error::{CoreError, CoreResult};

const ENTROPY: &[u8] = b"Local Transcript voice embedding v1";

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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err(CoreError::Security(
            "voice profiles require Windows DPAPI".into(),
        ))
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
