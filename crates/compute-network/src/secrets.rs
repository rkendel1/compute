//! Node-local secrets.
//!
//! Private keys never enter control state. They live in a directory only
//! the daemon's user can read, and control state holds a reference:
//! `node:<node id>/<name>`. A reference to another node's secret resolves
//! to nothing here, so a node that cannot find a key issues a new one
//! rather than reading one it does not hold.

use std::io;
use std::path::{Path, PathBuf};

pub struct SecretStore {
    root: PathBuf,
    node_id: String,
}

impl SecretStore {
    /// Open (creating) the store at `root` for `node_id`.
    pub fn open(root: impl Into<PathBuf>, node_id: impl Into<String>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        restrict(&root, 0o700)?;
        Ok(Self {
            root,
            node_id: node_id.into(),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The reference control state may hold for `name`.
    pub fn reference(&self, name: &str) -> String {
        format!("node:{}/{name}", self.node_id)
    }

    /// Store `bytes` under `name` and return its reference.
    pub fn put(&self, name: &str, bytes: &[u8]) -> io::Result<String> {
        let path = self.path(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict(parent, 0o700)?;
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, bytes)?;
        restrict(&temporary, 0o600)?;
        std::fs::rename(&temporary, &path)?;
        Ok(self.reference(name))
    }

    /// The secret a reference names, when this node holds it.
    pub fn get(&self, reference: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(name) = self.local_name(reference) else {
            return Ok(None);
        };
        match std::fs::read(self.path(name)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn get_named(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.get(&self.reference(name))
    }

    pub fn remove(&self, reference: &str) -> io::Result<()> {
        if let Some(name) = self.local_name(reference) {
            match std::fs::remove_file(self.path(name)?) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }

    fn local_name<'a>(&self, reference: &'a str) -> Option<&'a str> {
        reference
            .strip_prefix("node:")?
            .strip_prefix(self.node_id.as_str())?
            .strip_prefix('/')
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        let relative = Path::new(name);
        let safe = !name.is_empty()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !safe {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid secret name {name:?}"),
            ));
        }
        Ok(self.root.join(relative))
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_private_and_only_resolve_on_their_node() {
        let root = tempfile::tempdir().unwrap();
        let store = SecretStore::open(root.path().join("secrets"), "node-a").unwrap();
        let reference = store.put("tls/example.com/key.pem", b"secret").unwrap();
        assert_eq!(reference, "node:node-a/tls/example.com/key.pem");
        assert_eq!(store.get(&reference).unwrap().unwrap(), b"secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.path().join("secrets/tls/example.com/key.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let other = SecretStore::open(root.path().join("secrets"), "node-b").unwrap();
        assert!(other.get(&reference).unwrap().is_none());
        assert!(store.put("../escape", b"x").is_err());
        store.remove(&reference).unwrap();
        assert!(store.get(&reference).unwrap().is_none());
    }
}
