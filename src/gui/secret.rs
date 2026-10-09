//! Stream keys in Windows Credential Manager, one per destination. Windows
//! encrypts them with the user's login, so they never sit in a plain file.
//! Stored as UTF-16 like Windows' own passwords, so they can also be seen
//! and edited in Control Panel > Credential Manager (Windows Credentials).

use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{HSTRING, PWSTR};

fn target(destination: &str) -> String {
    format!("MilerCast/{destination}")
}

pub fn load(destination: &str) -> Option<String> {
    let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        CredReadW(&HSTRING::from(target(destination)), CRED_TYPE_GENERIC, None, &mut credential).ok()?;
        let c = &*credential;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        CredFree(credential as *const _);
        String::from_utf16(&units).ok().filter(|key| !key.is_empty())
    }
}

pub fn save(destination: &str, key: &str) {
    let mut target: Vec<u16> = target(destination).encode_utf16().chain([0]).collect();
    let mut user: Vec<u16> = "stream key".encode_utf16().chain([0]).collect();
    let mut blob: Vec<u8> = key.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        UserName: PWSTR(user.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe {
        let _ = CredWriteW(&credential, 0);
    }
}

pub fn delete(destination: &str) {
    unsafe {
        let _ = CredDeleteW(&HSTRING::from(target(destination)), CRED_TYPE_GENERIC, None);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn saves_loads_and_deletes_a_key() {
        let destination = "unit-test-only";
        super::save(destination, "live_123_abcñ");
        assert_eq!(super::load(destination).as_deref(), Some("live_123_abcñ"));
        super::delete(destination);
        assert_eq!(super::load(destination), None);
    }
}
