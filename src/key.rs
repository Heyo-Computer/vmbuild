//! The cache key.
//!
//! Keyed on the built image's **diffID chain**, not its config digest. M0
//! measured why: two identical cached builds produce byte-identical
//! `RootFS.Layers` but *different* config digests, because the config embeds
//! `created`/`history` timestamps. Keying on the config would mean the cache
//! never hits.
//!
//! Note what is deliberately *not* in the key: anything about the build
//! inputs. `--target`, `--build-arg`, `--platform` and the context are all
//! fed to BuildKit, and any difference they make shows up in the diffIDs.
//! Hashing them again would create a second key that can disagree with
//! BuildKit's own view of the build -- which is the failure mode that makes a
//! cache serve the wrong image.

use sha2::{Digest, Sha256};

/// Bump to invalidate every cached image, e.g. after changing how the ext4 is
/// laid out. Cheaper and less error-prone than trying to migrate entries.
const KEY_VERSION: &str = "vmbuild.ext4.v1";

/// The parts of the ext4 recipe that change the bytes we write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeKey {
    pub diff_ids: Vec<String>,
    /// A description of the *policy*, not its resolved value: "auto" resolves
    /// from the tar size, which is itself a function of the same layers that
    /// produced `diff_ids`. Hashing the resolved bytes would require exporting
    /// the rootfs before we could even look in the cache.
    pub size_policy: String,
    pub label: Option<String>,
    pub journal: bool,
    pub epoch_secs: u32,
}

impl RecipeKey {
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(KEY_VERSION.as_bytes());
        h.update([0]);
        for d in &self.diff_ids {
            h.update(d.as_bytes());
            h.update([0]);
        }
        h.update(b"|size=");
        h.update(self.size_policy.as_bytes());
        h.update(b"|label=");
        h.update(self.label.as_deref().unwrap_or("").as_bytes());
        h.update(b"|journal=");
        h.update([self.journal as u8]);
        h.update(b"|epoch=");
        h.update(self.epoch_secs.to_le_bytes());
        format!("{:x}", h.finalize())
    }

    /// A UUID derived from the key, so the image is byte-reproducible *and*
    /// two different recipes never collide on filesystem UUID.
    pub fn uuid(&self) -> uuid::Uuid {
        let d = self.digest();
        let bytes = hex16(&d);
        uuid::Uuid::from_bytes(bytes)
    }
}

fn hex16(hex: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let s = hex.get(i * 2..i * 2 + 2).unwrap_or("00");
        *slot = u8::from_str_radix(s, 16).unwrap_or(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> RecipeKey {
        RecipeKey {
            diff_ids: vec!["sha256:aaa".into(), "sha256:bbb".into()],
            size_policy: "auto".into(),
            label: Some("rootfs".into()),
            journal: true,
            epoch_secs: 1_577_836_800,
        }
    }

    #[test]
    fn stable_across_calls() {
        assert_eq!(k().digest(), k().digest());
    }

    #[test]
    fn layer_order_matters() {
        let mut b = k();
        b.diff_ids.reverse();
        assert_ne!(k().digest(), b.digest(), "chain order must be significant");
    }

    #[test]
    fn every_field_changes_the_key() {
        let base = k().digest();
        let mut a = k();
        a.size_policy = "fixed:123".into();
        assert_ne!(base, a.digest());
        let mut b = k();
        b.label = None;
        assert_ne!(base, b.digest());
        let mut c = k();
        c.journal = false;
        assert_ne!(base, c.digest());
        let mut d = k();
        d.epoch_secs += 1;
        assert_ne!(base, d.digest());
        let mut e = k();
        e.diff_ids.push("sha256:ccc".into());
        assert_ne!(base, e.digest());
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        // Without separators, ("ab","c") and ("a","bc") would collide.
        let mut a = k();
        a.label = Some("ab".into());
        a.size_policy = "c".into();
        let mut b = k();
        b.label = Some("a".into());
        b.size_policy = "bc".into();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn uuid_follows_the_key() {
        assert_eq!(k().uuid(), k().uuid());
        let mut other = k();
        other.diff_ids.push("sha256:zzz".into());
        assert_ne!(k().uuid(), other.uuid());
    }
}
