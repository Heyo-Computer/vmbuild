/// How large the ext4 image should be.
///
/// `arcbox_ext4::Formatter` needs the size at construction and its `close()`
/// only ever *grows* it, never shrinks -- so an over-estimate becomes the
/// final apparent size. That is not free downstream: heyvm's per-VM
/// `reflink_or_copy` falls back to a dense copy on ext4 hosts, and
/// `heyvm mvm build` streams the image to S3 where holes read as zeros.
/// Keep this tight.
#[derive(Debug, Clone)]
pub enum SizePolicy {
    /// Exactly this many bytes (rounded up to a block-group boundary by ext4).
    Fixed(u64),
    /// Derived from the uncompressed size of the source tar. Matches the
    /// heuristic heyvm's `image_builder.rs` uses today, so image sizes do not
    /// change under the migration.
    FromTar { tar_bytes: u64 },
}

/// `tar_bytes * 1.2 + 64 MiB`, floor 128 MiB -- heyvm's existing heuristic
/// (`image_builder.rs:473-479`).
pub const SLACK_RATIO: f64 = 1.2;
pub const HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
pub const MIN_BYTES: u64 = 128 * 1024 * 1024;

impl SizePolicy {
    pub fn resolve(&self) -> u64 {
        match *self {
            SizePolicy::Fixed(n) => n,
            SizePolicy::FromTar { tar_bytes } => {
                let est = (tar_bytes as f64 * SLACK_RATIO) as u64 + HEADROOM_BYTES;
                est.max(MIN_BYTES)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_tars_hit_the_floor() {
        assert_eq!(SizePolicy::FromTar { tar_bytes: 0 }.resolve(), MIN_BYTES);
        assert_eq!(SizePolicy::FromTar { tar_bytes: 1024 }.resolve(), MIN_BYTES);
    }

    #[test]
    fn large_tars_scale_with_slack() {
        // 1 GiB tar -> 1.2 GiB + 64 MiB.
        let gib = 1024 * 1024 * 1024u64;
        let got = SizePolicy::FromTar { tar_bytes: gib }.resolve();
        assert_eq!(got, (gib as f64 * 1.2) as u64 + HEADROOM_BYTES);
        assert!(got > gib, "must leave room for the extracted tree");
    }

    #[test]
    fn fixed_is_verbatim() {
        assert_eq!(SizePolicy::Fixed(777).resolve(), 777);
    }
}
