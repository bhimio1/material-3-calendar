//! Refresh tokens and passwords live in the Secret Service keyring (GNOME
//! Keyring, KWallet's Secret Service, KeePassXC, …) through `secret-tool`
//! from libsecret, never in a plain file.

use anyhow::{anyhow, Result};
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

pub trait Secrets: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, label: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// `secret-tool` with attributes `service=m3-calendar key=<key>`.
pub struct Keyring;

const SERVICE: &str = "m3-calendar";

impl Secrets for Keyring {
    fn get(&self, key: &str) -> Result<Option<String>> {
        if !material_3_ui::sys::cmd::exists("secret-tool") {
            return Err(anyhow!("secret-tool isn't installed (it comes with libsecret)"));
        }
        match material_3_ui::sys::cmd::run("secret-tool", &["lookup", "service", SERVICE, "key", key]) {
            Ok(v) if v.is_empty() => Ok(None),
            Ok(v) => Ok(Some(v.trim_end_matches('\n').to_string())),
            // lookup exits 1 without output when there's no such secret.
            Err(e) if e.contains("exited with") => Ok(None),
            Err(e) => Err(anyhow!("keyring: {e}")),
        }
    }

    fn set(&self, key: &str, label: &str, value: &str) -> Result<()> {
        material_3_ui::sys::cmd::run_with_stdin(
            "secret-tool",
            &["store", &format!("--label={label}"), "service", SERVICE, "key", key],
            value.as_bytes(),
        )
        .map(|_| ())
        .map_err(|e| anyhow!("couldn't save to the keyring: {e}"))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let _ = material_3_ui::sys::cmd::run("secret-tool", &["clear", "service", SERVICE, "key", key]);
        Ok(())
    }
}

/// For tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory(Mutex<HashMap<String, String>>);

#[cfg(test)]
impl Secrets for Memory {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, _label: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}
