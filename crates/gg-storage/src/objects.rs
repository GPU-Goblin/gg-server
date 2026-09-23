//! Content-addressed object store on a local durable directory.
//!
//! Layout: `<root>/objects/sha256/aa/bb/<hex>`; temporary files in `<root>/tmp`. An object becomes
//! visible only after its bytes are fully written, fsynced, hashed and atomically renamed.

use std::path::{Path, PathBuf};

use gg_core::Sha256Digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::StorageError;

/// A stored object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredObject {
    /// Hash.
    pub hash: Sha256Digest,
    /// Size in bytes.
    pub size: u64,
    /// Path on disk.
    pub path: PathBuf,
}

/// Local object store.
#[derive(Clone, Debug)]
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    /// Open (creating directories) a store rooted at `root`.
    pub fn open(root: &Path) -> Result<Self, StorageError> {
        std::fs::create_dir_all(root.join("objects/sha256"))?;
        std::fs::create_dir_all(root.join("tmp"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of an object (system-controlled; never derived from user-provided names).
    pub fn path_for(&self, hash: &Sha256Digest) -> PathBuf {
        let hex = hash.to_hex();
        self.root
            .join("objects/sha256")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    /// Delete a stored object (an artifact withdrawn by an operator). Returns whether it existed.
    pub async fn remove(&self, hash: &Sha256Digest) -> Result<bool, StorageError> {
        match tokio::fs::remove_file(self.path_for(hash)).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether the object exists.
    pub fn exists(&self, hash: &Sha256Digest) -> bool {
        self.path_for(hash).is_file()
    }

    /// Size of a stored object.
    pub async fn size(&self, hash: &Sha256Digest) -> Result<u64, StorageError> {
        let meta = tokio::fs::metadata(self.path_for(hash))
            .await
            .map_err(|_| StorageError::NotFound("object"))?;
        Ok(meta.len())
    }

    /// Open a stored object for streaming reads.
    pub async fn open_read(&self, hash: &Sha256Digest) -> Result<tokio::fs::File, StorageError> {
        tokio::fs::File::open(self.path_for(hash))
            .await
            .map_err(|_| StorageError::NotFound("object"))
    }

    /// Store bytes from a reader. `expected` (hash, size) is checked when provided; `max_size`
    /// bounds the upload regardless. Returns the stored object; storing an existing object is a
    /// no-op that still verifies the bytes.
    pub async fn put_stream<R: AsyncRead + Unpin>(
        &self,
        mut reader: R,
        expected: Option<(Sha256Digest, u64)>,
        max_size: u64,
    ) -> Result<StoredObject, StorageError> {
        let limit = expected.map(|(_, s)| s.min(max_size)).unwrap_or(max_size);
        let tmp_path = self
            .root
            .join("tmp")
            .join(format!("upload-{}", uuid::Uuid::now_v7()));
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut hasher = Sha256Digest::hasher();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 256 * 1024];
        let result: Result<(), StorageError> = async {
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > limit {
                    return Err(StorageError::TooLarge(total, limit));
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).await?;
            }
            file.sync_all().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }
        drop(file);
        let hash = hasher.finalize();
        if let Some((exp_hash, exp_size)) = expected {
            if total != exp_size {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(StorageError::SizeMismatch {
                    expected: exp_size,
                    actual: total,
                });
            }
            if hash != exp_hash {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(StorageError::HashMismatch {
                    expected: exp_hash,
                    actual: hash,
                });
            }
        }
        let final_path = self.path_for(&hash);
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if final_path.exists() {
            let _ = tokio::fs::remove_file(&tmp_path).await;
        } else {
            tokio::fs::rename(&tmp_path, &final_path).await?;
            // fsync the directory so the rename is durable
            if let Some(parent) = final_path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all();
            }
        }
        Ok(StoredObject {
            hash,
            size: total,
            path: final_path,
        })
    }

    /// Store in-memory bytes from a blocking context (`spawn_blocking`), with the guarantees of
    /// [`Self::put_stream`]: fsynced, hashed, atomically renamed. The hash is computed on a second
    /// thread while the bytes are written, so a large object costs its write, not write plus hash.
    pub fn put_bytes_blocking(
        &self,
        bytes: &[u8],
        max_size: u64,
    ) -> Result<StoredObject, StorageError> {
        use std::io::Write;
        if bytes.len() as u64 > max_size {
            return Err(StorageError::TooLarge(bytes.len() as u64, max_size));
        }
        let tmp_path = self
            .root
            .join("tmp")
            .join(format!("upload-{}", uuid::Uuid::now_v7()));
        let (hash, written) = std::thread::scope(|s| {
            let hasher = s.spawn(|| Sha256Digest::of_bytes(bytes));
            let written = std::fs::File::create(&tmp_path)
                .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()));
            (hasher.join().expect("hashing thread"), written)
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        let final_path = self.path_for(&hash);
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if final_path.exists() {
            let _ = std::fs::remove_file(&tmp_path);
        } else {
            std::fs::rename(&tmp_path, &final_path)?;
            // fsync the directory so the rename is durable
            if let Some(parent) = final_path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all();
            }
        }
        Ok(StoredObject {
            hash,
            size: bytes.len() as u64,
            path: final_path,
        })
    }

    /// [`Self::read_bytes`] from a blocking context.
    pub fn read_bytes_blocking(
        &self,
        hash: &Sha256Digest,
        max_size: u64,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.path_for(hash);
        let size = std::fs::metadata(&path)
            .map_err(|_| StorageError::NotFound("object"))?
            .len();
        if size > max_size {
            return Err(StorageError::TooLarge(size, max_size));
        }
        let bytes = std::fs::read(&path)?;
        let actual = Sha256Digest::of_bytes(&bytes);
        if &actual != hash {
            return Err(StorageError::HashMismatch {
                expected: *hash,
                actual,
            });
        }
        Ok(bytes)
    }

    /// Store in-memory bytes.
    pub async fn put_bytes(
        &self,
        bytes: &[u8],
        max_size: u64,
    ) -> Result<StoredObject, StorageError> {
        self.put_stream(bytes, None, max_size).await
    }

    /// Read a whole object into memory, refusing objects larger than `max_size`.
    pub async fn read_bytes(
        &self,
        hash: &Sha256Digest,
        max_size: u64,
    ) -> Result<Vec<u8>, StorageError> {
        let size = self.size(hash).await?;
        if size > max_size {
            return Err(StorageError::TooLarge(size, max_size));
        }
        let bytes = tokio::fs::read(self.path_for(hash)).await?;
        let actual = Sha256Digest::of_bytes(&bytes);
        if &actual != hash {
            return Err(StorageError::HashMismatch {
                expected: *hash,
                actual,
            });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_verify_and_reject() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(dir.path()).unwrap();
        let data = b"hello object".to_vec();
        let h = Sha256Digest::of_bytes(&data);
        let o = store
            .put_stream(&data[..], Some((h, data.len() as u64)), 1024)
            .await
            .unwrap();
        assert_eq!(o.hash, h);
        assert!(store.exists(&h));
        assert_eq!(store.read_bytes(&h, 1024).await.unwrap(), data);
        // wrong announced hash
        let err = store
            .put_stream(
                &data[..],
                Some((Sha256Digest::zero(), data.len() as u64)),
                1024,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::HashMismatch { .. }));
        // wrong announced size
        let err = store
            .put_stream(&data[..], Some((h, 3)), 1024)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::TooLarge(..) | StorageError::SizeMismatch { .. }
        ));
        // over max
        let err = store.put_stream(&data[..], None, 4).await.unwrap_err();
        assert!(matches!(err, StorageError::TooLarge(..)));
        // no temp files left behind
        assert_eq!(
            std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
            0
        );
        // idempotent put
        let o2 = store.put_bytes(&data, 1024).await.unwrap();
        assert_eq!(o2, o);
        // corrupted file on disk is detected on read
        std::fs::write(store.path_for(&h), b"hello objecT").unwrap();
        assert!(matches!(
            store.read_bytes(&h, 1024).await,
            Err(StorageError::HashMismatch { .. })
        ));
        assert!(matches!(
            store.read_bytes_blocking(&h, 1024),
            Err(StorageError::HashMismatch { .. })
        ));
    }

    #[test]
    fn blocking_put_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(dir.path()).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let o = store.put_bytes_blocking(&data, 1 << 20).unwrap();
        assert_eq!(o.hash, Sha256Digest::of_bytes(&data));
        assert_eq!(o.size, data.len() as u64);
        assert_eq!(std::fs::read(&o.path).unwrap(), data);
        assert_eq!(store.read_bytes_blocking(&o.hash, 1 << 20).unwrap(), data);
        // Again: a no-op that returns the same object.
        assert_eq!(store.put_bytes_blocking(&data, 1 << 20).unwrap(), o);
        assert!(matches!(
            store.put_bytes_blocking(&data, 1000),
            Err(StorageError::TooLarge(..))
        ));
        assert!(matches!(
            store.read_bytes_blocking(&o.hash, 1000),
            Err(StorageError::TooLarge(..))
        ));
        assert!(matches!(
            store.read_bytes_blocking(&Sha256Digest::zero(), 1 << 20),
            Err(StorageError::NotFound(_))
        ));
        assert_eq!(
            std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
            0
        );
    }
}
