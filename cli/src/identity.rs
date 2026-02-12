use std::path::Path;

use anyhow::{Context, Result};
use libp2p::identity::Keypair;

/// Load a keypair from a protobuf-encoded file, or generate a new Ed25519
/// keypair and persist it for future runs.
pub fn load_or_generate(path: &Path) -> Result<Keypair> {
    match path.exists() {
        true => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading identity from {}", path.display()))?;
            let keypair = Keypair::from_protobuf_encoding(&bytes)
                .with_context(|| format!("decoding identity from {}", path.display()))?;
            tracing::info!(peer_id = %keypair.public().to_peer_id(), "loaded identity");
            Ok(keypair)
        }
        false => {
            let keypair = Keypair::generate_ed25519();
            let bytes = keypair
                .to_protobuf_encoding()
                .context("encoding identity")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {}", parent.display()))?;
            }
            std::fs::write(path, &bytes)
                .with_context(|| format!("writing identity to {}", path.display()))?;
            tracing::info!(peer_id = %keypair.public().to_peer_id(), "generated new identity");
            Ok(keypair)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_identity() {
        let dir = tempfile::tempdir().expect("create temp directory");
        let path = dir.path().join("identity.key");

        let kp1 = load_or_generate(&path).expect("generate identity keypair");
        let kp2 = load_or_generate(&path).expect("reload identity keypair");

        assert_eq!(
            kp1.public().to_peer_id(),
            kp2.public().to_peer_id(),
            "reloaded identity must match"
        );
    }

    #[test]
    fn generates_fresh_if_missing() {
        let dir = tempfile::tempdir().expect("create temp directory");
        let path = dir.path().join("subdir/identity.key");

        assert!(!path.exists());
        let kp = load_or_generate(&path).expect("generate identity in subdir");
        assert!(path.exists());
        assert_eq!(kp.key_type(), libp2p::identity::KeyType::Ed25519);
    }
}
