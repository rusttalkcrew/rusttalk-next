//! The private profile holds ciphertext. Only Android Keystore can open it.
use super::{Keychain, WRAPPED, remove, write_atomic};
use anyhow::{Context, Result};
use jni::{JValue, jni_sig, jni_str, objects::JByteArray, strings::JNIStr};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use zeroize::Zeroizing;

pub(super) struct AndroidKeystore {
    blob: PathBuf,
    alias: String,
    probe: OnceLock<Result<(), String>>,
}

impl AndroidKeystore {
    pub(super) fn new(config: &Path, service: &str) -> Self {
        Self {
            blob: config.join(WRAPPED),
            alias: format!("{service}/identity-v1"),
            probe: OnceLock::new(),
        }
    }

    fn command(&self, method: &JNIStr) -> Result<()> {
        let app = crate::android::app().context("no Android activity")?;
        crate::android::java(app, |env, activity| {
            let alias = env.new_string(&self.alias)?;
            env.call_method(
                activity,
                method,
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&alias)],
            )?
            .v()
        })
        .context("Android Keystore operation")
    }

    fn crypt(&self, encrypt: bool, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let app = crate::android::app().context("no Android activity")?;
        crate::android::java(app, |env, activity| {
            let alias = env.new_string(&self.alias)?;
            let input = env.byte_array_from_slice(bytes)?;
            let output = env
                .call_method(
                    activity,
                    jni_str!("cryptIdentity"),
                    jni_sig!("(Ljava/lang/String;Z[B)[B"),
                    &[
                        JValue::Object(&alias),
                        JValue::Bool(encrypt),
                        JValue::Object(&input),
                    ],
                )?
                .l()?;
            let output = env.cast_local::<JByteArray>(output)?;
            let result = Zeroizing::new(env.convert_byte_array(&output)?);
            if !encrypt {
                output.set_region(env, 0, &vec![0; result.len()])?;
            }
            Ok(result)
        })
        .context("wrap or unwrap Android identity")
    }
}

impl Keychain for AndroidKeystore {
    fn available(&self) -> Result<()> {
        self.probe
            .get_or_init(|| {
                self.command(jni_str!("probeIdentityKey"))
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(anyhow::Error::msg)
    }

    fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        match fs::read(&self.blob) {
            Ok(blob) => self.crypt(false, &blob).map(Some),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read wrapped Android identity"),
        }
    }

    fn set(&self, secret: &[u8]) -> Result<()> {
        self.available()?;
        write_atomic(&self.blob, &self.crypt(true, secret)?)?;
        fs::File::open(self.blob.parent().context("profile folder")?)?.sync_all()?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        self.command(jni_str!("deleteIdentityKey"))?;
        remove(&self.blob)
    }
}
