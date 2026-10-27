//! Explicit, temporary CurrentUser ROOT trust. Never touches LocalMachine.
//! A durable journal and per-certificate random ownership property prevent us
//! from removing trust installed independently by the user or another data root.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};

const JOURNAL: &str = "claudecodetrust";
#[derive(Serialize, Deserialize)]
struct Lease {
    der: Vec<u8>,
    owner: String,
}
#[derive(Default)]
struct Presence {
    present: bool,
    owned: bool,
}
trait Store: Send + Sync {
    fn inspect(&self, der: &[u8], owner: Option<&str>) -> Result<Presence, String>;
    fn add(&self, der: &[u8], owner: &str) -> Result<(), String>;
    fn remove(&self, der: &[u8], owner: &str) -> Result<(), String>;
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustStatus {
    pub supported: bool,
    pub sha256: Option<String>,
    pub trusted: bool,
    pub managed: bool,
    pub recovery_required: bool,
}
pub struct CodeTrust {
    root: PathBuf,
    store: Arc<dyn Store>,
}
impl CodeTrust {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            store: Arc::new(PlatformStore),
        }
    }
    #[cfg(test)]
    pub(super) fn isolated(root: PathBuf) -> Self {
        // Constructing the native adapter does no I/O; replace it before use.
        let mut trust = Self::new(root);
        trust.store = Arc::new(tests::FakeStore::default());
        trust
    }
    fn lease(&self) -> Result<Option<Lease>, String> {
        crate::credentials::load(&self.root, JOURNAL)
            .map_err(|_| "Cannot decrypt Code CA trust journal")?
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| "Invalid Code CA trust journal".into())
            })
            .transpose()
    }
    fn clear(&self) -> Result<(), String> {
        crate::credentials::remove(&self.root, JOURNAL)
            .map_err(|_| "Cannot clear Code CA trust journal".into())
    }
    pub fn has_lease(&self) -> Result<bool, String> {
        Ok(self.lease()?.is_some())
    }
    pub fn status(&self, der: Option<&[u8]>, running: bool) -> Result<TrustStatus, String> {
        let lease = self.lease()?;
        let selected = lease.as_ref().map(|l| l.der.as_slice()).or(der);
        let presence = selected
            .map(|der| {
                self.store
                    .inspect(der, lease.as_ref().map(|l| l.owner.as_str()))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(TrustStatus {
            supported: cfg!(windows),
            sha256: selected.map(fingerprint),
            trusted: presence.present,
            managed: lease.is_some(),
            recovery_required: lease.is_some() && (!running || !presence.owned),
        })
    }
    pub fn enable(&self, der: &[u8], expected: &str) -> Result<(), String> {
        if fingerprint(der) != expected {
            return Err("CA fingerprint changed; review the certificate again".into());
        }
        if let Some(lease) = self.lease()? {
            if lease.der == der && self.store.inspect(der, Some(&lease.owner))?.owned {
                return Ok(());
            }
            return Err("Restore the previous temporary CA trust first".into());
        }
        // Existing independent trust is never adopted, journaled or removed.
        if self.store.inspect(der, None)?.present {
            return Ok(());
        }
        let lease = Lease {
            der: der.to_vec(),
            owner: super::accounts::random_secret()?,
        };
        crate::credentials::save(
            &self.root,
            JOURNAL,
            &serde_json::to_string(&lease).map_err(|_| "Cannot encode Code CA trust journal")?,
        )
        .map_err(|_| "Cannot save Code CA trust journal")?;
        // Journal first. The native insertion carries the ownership property;
        // ambiguous or externally changed entries are never removed.
        self.store.add(der, &lease.owner)?;
        let persisted = self.store.inspect(der, Some(&lease.owner))?;
        if !persisted.present {
            return Err("Windows reported a successful CA import, but the certificate is absent after reopening CurrentUser ROOT. Routing was not enabled; restore temporary trust and check the Windows certificate store".into());
        }
        if !persisted.owned {
            return Err("CA trust could not be verified; restore temporary trust".into());
        }
        Ok(())
    }
    pub fn validate_for_routing(&self, der: &[u8]) -> Result<(), String> {
        if let Some(lease) = self.lease()? {
            if lease.der != der || !self.store.inspect(&lease.der, Some(&lease.owner))?.owned {
                return Err(
                    "Restore or repair temporary CA ownership before starting Code routing".into(),
                );
            }
        }
        Ok(())
    }
    pub fn restore(&self) -> Result<(), String> {
        let Some(lease) = self.lease()? else {
            return Ok(());
        };
        self.store.remove(&lease.der, &lease.owner)?;
        self.clear()
    }
}
fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

struct PlatformStore;
#[cfg(not(windows))]
impl Store for PlatformStore {
    fn inspect(&self, _: &[u8], _: Option<&str>) -> Result<Presence, String> {
        Ok(Presence::default())
    }
    fn add(&self, _: &[u8], _: &str) -> Result<(), String> {
        Err("Temporary CA trust is supported only on Windows".into())
    }
    fn remove(&self, _: &[u8], _: &str) -> Result<(), String> {
        Err("Restore this CA trust on the original Windows host".into())
    }
}

#[cfg(windows)]
mod windows_store {
    use super::*;
    use windows_sys::Win32::Security::Cryptography::*;
    const OWNER_PROPERTY: u32 = CERT_FIRST_USER_PROP_ID + 189;
    struct StoreHandle(HCERTSTORE);
    impl Drop for StoreHandle {
        fn drop(&mut self) {
            unsafe {
                CertCloseStore(self.0, 0);
            }
        }
    }
    struct Context(*mut CERT_CONTEXT);
    impl Drop for Context {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CertFreeCertificateContext(self.0);
                }
            }
        }
    }
    fn open(write: bool) -> Result<StoreHandle, String> {
        let name: Vec<u16> = "ROOT\0".encode_utf16().collect();
        let flags = CERT_SYSTEM_STORE_CURRENT_USER
            | CERT_STORE_OPEN_EXISTING_FLAG
            | if write { 0 } else { CERT_STORE_READONLY_FLAG };
        // SAFETY: NUL-terminated store name, no borrowed native handle retained.
        let handle =
            unsafe { CertOpenStore(CERT_STORE_PROV_SYSTEM_W, 0, 0, flags, name.as_ptr().cast()) };
        if handle.is_null() {
            return Err("Cannot open Windows CurrentUser ROOT store".into());
        }
        Ok(StoreHandle(handle))
    }
    fn open_registry_root() -> Result<StoreHandle, String> {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
        };
        let path: Vec<u16> = "Software\\Microsoft\\SystemCertificates\\Root\0"
            .encode_utf16()
            .collect();
        let mut key = std::ptr::null_mut();
        let error = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_READ | KEY_WRITE,
                &mut key,
            )
        };
        if error != 0 {
            return Err(format!(
                "Cannot open Windows CurrentUser ROOT registry store ({error:#x})"
            ));
        }
        // The registry provider duplicates the handle before returning.
        let handle = unsafe { CertOpenStore(CERT_STORE_PROV_REG, 0, 0, 0, key.cast()) };
        unsafe { RegCloseKey(key) };
        if handle.is_null() {
            return Err("Cannot open Windows CurrentUser ROOT physical store".into());
        }
        Ok(StoreHandle(handle))
    }
    fn protected_root_approved(der: &[u8]) -> Result<bool, String> {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY,
        };
        let context = Context(unsafe {
            CertCreateCertificateContext(X509_ASN_ENCODING, der.as_ptr(), der.len() as u32)
        });
        if context.0.is_null() {
            return Err("Cannot inspect approved CA fingerprint".into());
        }
        let mut hash = [0u8; 20];
        let mut hash_size = hash.len() as u32;
        if unsafe {
            CertGetCertificateContextProperty(
                context.0,
                CERT_SHA1_HASH_PROP_ID,
                hash.as_mut_ptr().cast(),
                &mut hash_size,
            )
        } == 0
            || hash_size != hash.len() as u32
        {
            return Err("Cannot inspect approved CA fingerprint".into());
        }
        let path: Vec<u16> = "Software\\Microsoft\\SystemCertificates\\Root\\ProtectedRoots\0"
            .encode_utf16()
            .collect();
        let name: Vec<u16> = "Certificates\0".encode_utf16().collect();
        let mut size = 0;
        let read = |buffer: *mut std::ffi::c_void, size: &mut u32| unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_BINARY,
                std::ptr::null_mut(),
                buffer,
                size,
            )
        };
        if read(std::ptr::null_mut(), &mut size) != 0 || size > 1024 * 1024 {
            return Ok(false);
        }
        let mut data = vec![0u8; size as usize];
        if read(data.as_mut_ptr().cast(), &mut size) != 0 {
            return Ok(false);
        }
        data.truncate(size as usize);
        Ok(approved_hash_list_contains(&data, &hash))
    }
    fn approved_hash_list_contains(data: &[u8], hash: &[u8; 20]) -> bool {
        // Fail closed on unknown Windows formats; never rewrite this list.
        if data.len() < 24 || data[..4] != 24u32.to_le_bytes() {
            return false;
        }
        let count = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
        data.len().checked_sub(24) == count.checked_mul(20)
            && data[24..]
                .as_chunks::<20>()
                .0
                .iter()
                .any(|entry| entry == hash)
    }
    fn find(store: &StoreHandle, der: &[u8]) -> Result<Option<Context>, String> {
        let mut previous = std::ptr::null_mut();
        loop {
            // Enumeration releases the previous context, including at EOF.
            let next = unsafe { CertEnumCertificatesInStore(store.0, previous) };
            if next.is_null() {
                let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                if error != windows_sys::Win32::Foundation::CRYPT_E_NOT_FOUND as u32 {
                    return Err("Cannot enumerate Windows roots".into());
                }
                return Ok(None);
            }
            let bytes = unsafe {
                std::slice::from_raw_parts((*next).pbCertEncoded, (*next).cbCertEncoded as usize)
            };
            if bytes == der {
                return Ok(Some(Context(next)));
            }
            previous = next;
        }
    }
    fn owned(context: &Context, owner: &str) -> bool {
        let mut size = 0;
        if unsafe {
            CertGetCertificateContextProperty(
                context.0,
                OWNER_PROPERTY,
                std::ptr::null_mut(),
                &mut size,
            )
        } == 0
            || size != owner.len() as u32
        {
            return false;
        }
        let mut value = vec![0u8; size as usize];
        (unsafe {
            CertGetCertificateContextProperty(
                context.0,
                OWNER_PROPERTY,
                value.as_mut_ptr().cast(),
                &mut size,
            ) != 0
        }) && value == owner.as_bytes()
    }
    impl Store for PlatformStore {
        fn inspect(&self, der: &[u8], owner: Option<&str>) -> Result<Presence, String> {
            let store = open(false)?;
            let context = find(&store, der)?;
            Ok(Presence {
                present: context.is_some(),
                owned: context
                    .as_ref()
                    .is_some_and(|c| owner.is_some_and(|o| owned(c, o))),
            })
        }
        fn add(&self, der: &[u8], owner: &str) -> Result<(), String> {
            let store = open(true)?;
            add_owned(&store, der, owner)?;
            drop(store);
            if find(&open(false)?, der)?.is_some() {
                return Ok(());
            }
            if !protected_root_approved(der)? {
                return Err(
                    "Windows did not retain approval for this CA; physical repair was refused"
                        .into(),
                );
            }
            // A broken protected-root cache can accept the add yet leave no
            // registry entry. The normal add has already registered this
            // fingerprint with Windows. Restore only this owned certificate
            // through the documented physical registry provider, then let the
            // caller verify it again through the ordinary logical Root store.
            let physical = open_registry_root()?;
            if find(&physical, der)?.is_some() {
                return Err("CA appeared in the physical Root store unexpectedly".into());
            }
            add_owned(&physical, der, owner)
        }
        fn remove(&self, der: &[u8], owner: &str) -> Result<(), String> {
            let store = open(true)?;
            let Some(mut context) = find(&store, der)? else {
                return Ok(());
            };
            if !owned(&context, owner) {
                return Err("CA ownership changed or insertion was interrupted; inspect this fingerprint in Windows CurrentUser ROOT manually".into());
            }
            let raw = std::mem::replace(&mut context.0, std::ptr::null_mut());
            if unsafe { CertDeleteCertificateFromStore(raw) } == 0 {
                return Err("Cannot remove temporary Code CA trust".into());
            }
            Ok(())
        }
    }
    fn add_owned(store: &StoreHandle, der: &[u8], owner: &str) -> Result<(), String> {
        // Tag a detached context before insertion. Some system-store
        // providers reject property changes on the returned store context.
        let prepared = Context(unsafe {
            CertCreateCertificateContext(X509_ASN_ENCODING, der.as_ptr(), der.len() as u32)
        });
        if prepared.0.is_null() {
            return Err("Cannot prepare temporary CA context".into());
        }
        let property = CRYPT_INTEGER_BLOB {
            cbData: owner.len() as u32,
            pbData: owner.as_ptr().cast_mut(),
        };
        if unsafe {
            CertSetCertificateContextProperty(
                prepared.0,
                OWNER_PROPERTY,
                0,
                (&property as *const CRYPT_INTEGER_BLOB).cast(),
            )
        } == 0
        {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(format!(
                "Cannot prepare CA ownership (Windows {error:#x}); trust was not added"
            ));
        }
        let mut context = Context(std::ptr::null_mut());
        if unsafe {
            CertAddCertificateContextToStore(
                store.0,
                prepared.0,
                CERT_STORE_ADD_NEW,
                &mut context.0,
            )
        } == 0
        {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(format!(
                "Cannot add temporary CA (Windows {error:#x}); existing trust was preserved"
            ));
        }
        if !owned(&context, owner) {
            let raw = std::mem::replace(&mut context.0, std::ptr::null_mut());
            // Delete only the context returned by our successful ADD_NEW.
            if unsafe { CertDeleteCertificateFromStore(raw) } == 0 {
                return Err(
                    "CA ownership copy and rollback failed; inspect Windows ROOT manually".into(),
                );
            }
            return Err("CA ownership was not copied; insertion rolled back".into());
        }
        Ok(())
    }
    #[test]
    fn physical_repair_requires_exact_approved_hash_and_known_format() {
        let hash = [7u8; 20];
        let mut data = vec![0u8; 24];
        data[..4].copy_from_slice(&24u32.to_le_bytes());
        data[16..20].copy_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&hash);
        assert!(approved_hash_list_contains(&data, &hash));
        assert!(!approved_hash_list_contains(&data, &[8u8; 20]));
        assert!(!approved_hash_list_contains(&data[..43], &hash));
        data[0] = 25;
        assert!(!approved_hash_list_contains(&data, &hash));
    }
    #[test]
    fn native_insertion_copies_ownership_and_preserves_existing_entry() {
        // Memory provider only: no Windows trust or registry changes.
        let store = StoreHandle(unsafe {
            CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, std::ptr::null())
        });
        assert!(!store.0.is_null());
        let cert =
            vellum_proxy_runtime::claude_code::certificate::CodeCertificate::generate().unwrap();
        add_owned(&store, cert.ca_der(), "first-owner").unwrap();
        assert!(owned(
            &find(&store, cert.ca_der()).unwrap().unwrap(),
            "first-owner"
        ));
        assert!(add_owned(&store, cert.ca_der(), "second-owner").is_err());
        let context = find(&store, cert.ca_der()).unwrap().unwrap();
        assert!(owned(&context, "first-owner"));
        assert!(!owned(&context, "second-owner"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct NonPersistingStore;
    impl Store for NonPersistingStore {
        fn inspect(&self, _: &[u8], _: Option<&str>) -> Result<Presence, String> {
            Ok(Presence::default())
        }
        fn add(&self, _: &[u8], _: &str) -> Result<(), String> {
            Ok(())
        }
        fn remove(&self, _: &[u8], _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn successful_import_without_persistence_never_reports_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let trust = CodeTrust {
            root: dir.path().into(),
            store: Arc::new(NonPersistingStore),
        };
        let der = b"missing-after-import";
        let error = trust.enable(der, &fingerprint(der)).unwrap_err();
        assert!(error.contains("absent after reopening CurrentUser ROOT"));
        let status = trust.status(Some(der), false).unwrap();
        assert!(!status.trusted);
        assert!(status.recovery_required);
        trust.restore().unwrap();
        assert!(!trust.has_lease().unwrap());
    }
    #[derive(Default)]
    pub(super) struct FakeStore {
        entry: Mutex<Option<(Vec<u8>, Option<String>)>>,
    }
    impl Store for FakeStore {
        fn inspect(&self, der: &[u8], owner: Option<&str>) -> Result<Presence, String> {
            let entry = self.entry.lock().unwrap();
            Ok(Presence {
                present: entry.as_ref().is_some_and(|(d, _)| d == der),
                owned: entry.as_ref().is_some_and(|(d, o)| {
                    d == der && o.as_deref().is_some() && o.as_deref() == owner
                }),
            })
        }
        fn add(&self, der: &[u8], owner: &str) -> Result<(), String> {
            *self.entry.lock().unwrap() = Some((der.to_vec(), Some(owner.into())));
            Ok(())
        }
        fn remove(&self, der: &[u8], owner: &str) -> Result<(), String> {
            if !self.inspect(der, Some(owner))?.present {
                return Ok(());
            }
            if !self.inspect(der, Some(owner))?.owned {
                return Err("Ownership mismatch".into());
            }
            *self.entry.lock().unwrap() = None;
            Ok(())
        }
    }
    fn fixture() -> (tempfile::TempDir, CodeTrust, Arc<FakeStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeStore::default());
        let manager = CodeTrust {
            root: dir.path().into(),
            store: store.clone(),
        };
        (dir, manager, store)
    }
    #[test]
    fn owned_trust_recovers_after_restart_without_touching_other_roots() {
        let (_dir, trust, store) = fixture();
        let der = b"fake-ca";
        trust.enable(der, &fingerprint(der)).unwrap();
        trust.validate_for_routing(der).unwrap();
        assert!(trust.validate_for_routing(b"another-ca").is_err());
        assert!(trust.status(Some(der), true).unwrap().managed);
        let recovered = CodeTrust {
            root: trust.root.clone(),
            store,
        };
        assert!(
            recovered
                .status(Some(der), false)
                .unwrap()
                .recovery_required
        );
        recovered.restore().unwrap();
        assert!(!recovered.status(Some(der), false).unwrap().trusted);
    }
    #[test]
    fn preexisting_trust_is_never_owned_or_removed() {
        let (_dir, trust, store) = fixture();
        let der = b"fake-ca";
        *store.entry.lock().unwrap() = Some((der.to_vec(), None));
        trust.enable(der, &fingerprint(der)).unwrap();
        trust.restore().unwrap();
        assert!(trust.status(Some(der), false).unwrap().trusted);
        assert!(!trust.status(Some(der), false).unwrap().managed);
    }
    #[test]
    fn changed_fingerprint_or_ownership_does_not_remove_foreign_trust() {
        let (_dir, trust, store) = fixture();
        let der = b"fake-ca";
        assert!(trust.enable(der, "wrong").is_err());
        trust.enable(der, &fingerprint(der)).unwrap();
        *store.entry.lock().unwrap() = Some((der.to_vec(), None));
        assert!(trust.restore().is_err());
        assert!(trust.validate_for_routing(der).is_err());
        assert!(trust.status(Some(der), false).unwrap().recovery_required);
        *store.entry.lock().unwrap() = None;
        trust.restore().unwrap();
        assert!(trust.lease().unwrap().is_none());
    }

    #[tokio::test]
    async fn manager_ownership_is_held_until_temporary_trust_cleanup_finishes() {
        struct BlockingStore {
            inner: FakeStore,
            entered: std::sync::mpsc::SyncSender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl Store for BlockingStore {
            fn inspect(&self, der: &[u8], owner: Option<&str>) -> Result<Presence, String> {
                self.inner.inspect(der, owner)
            }
            fn add(&self, der: &[u8], owner: &str) -> Result<(), String> {
                self.inner.add(der, owner)
            }
            fn remove(&self, der: &[u8], owner: &str) -> Result<(), String> {
                let _ = self.entered.send(());
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5));
                self.inner.remove(der, owner)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let (entered, ready) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::channel();
        let store = Arc::new(BlockingStore {
            inner: FakeStore::default(),
            entered,
            release: Mutex::new(wait),
        });
        let mut manager = super::super::CodeManager::with_user_settings(root.clone(), None);
        Arc::get_mut(&mut manager).unwrap().trust = Arc::new(CodeTrust {
            root: root.clone(),
            store,
        });
        manager.start(project.clone()).await.unwrap();
        manager
            .trust
            .enable(b"fake-ca", &fingerprint(b"fake-ca"))
            .unwrap();
        let stopping = manager.clone();
        let task = tokio::spawn(async move { stopping.stop(false).await });
        let reached = tokio::task::spawn_blocking(move || {
            ready.recv_timeout(std::time::Duration::from_secs(3))
        })
        .await
        .unwrap();
        let other = super::super::CodeManager::with_user_settings(root, None);
        let result = other.start(project).await;
        let _ = release.send(());
        task.await.unwrap().unwrap();
        if result.is_ok() {
            other.stop(false).await.unwrap();
        }
        assert!(reached.is_ok(), "Trust cleanup was not reached");
        assert!(
            result.is_err(),
            "Another manager started while old trust was being removed"
        );
    }
}
