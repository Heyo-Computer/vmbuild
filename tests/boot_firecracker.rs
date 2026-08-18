//! End-to-end: does a real Firecracker microVM boot an image vmbuild wrote?
//!
//! Every other test asks e2fsprogs whether the filesystem is well-formed. This
//! one asks the actual consumer. It also exercises the journal for real: the
//! guest mounts rw, writes, and force-reboots, which leaves `needs_recovery`
//! set -- and heyvm's `grow_ext4_image` then runs `e2fsck -fp` over exactly
//! that state.
//!
//! Ignored by default: needs `/dev/kvm`, the `firecracker` binary, a kernel at
//! `~/.heyo/images/firecracker/vmlinux.bin`, and `docker` to produce a rootfs.
//!
//!     cargo test --release --test boot_firecracker -- --ignored --nocapture

mod common;

use common::have;
use std::path::PathBuf;
use std::process::Command;

fn kernel() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".heyo/images/firecracker/vmlinux.bin")
}

const INIT: &str = r#"#!/bin/sh
mount -t proc proc /proc 2>/dev/null
echo "VMBUILD_BOOT_OK uname=$(uname -r)"
echo "VMBUILD_RW=$(touch /rwtest && echo yes || echo no)"
sync
reboot -f
"#;

#[test]
#[ignore = "needs /dev/kvm, firecracker, a kernel image and docker"]
fn firecracker_boots_a_vmbuild_image() {
    if !have("firecracker") || !have("docker") || !kernel().exists() {
        eprintln!("skipping: firecracker/docker/kernel unavailable");
        return;
    }
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        eprintln!("skipping: no rw access to /dev/kvm");
        return;
    }

    let d = tempfile::tempdir().unwrap();

    // A minimal rootfs whose init prints a marker and powers off. init.sh is
    // COPY'd rather than heredoc'd into a RUN: its newlines would otherwise
    // terminate the Dockerfile instruction.
    std::fs::write(d.path().join("init.sh"), INIT).unwrap();
    std::fs::write(
        d.path().join("Dockerfile"),
        "FROM alpine:3.21\nCOPY init.sh /init.sh\nRUN chmod +x /init.sh\n",
    )
    .unwrap();

    let tar = d.path().join("rootfs.tar");
    let ok = Command::new("docker")
        .args(["buildx", "build", "-f"])
        .arg(d.path().join("Dockerfile"))
        .arg(format!("-o=type=tar,dest={}", tar.display()))
        .arg(d.path())
        .status()
        .unwrap()
        .success();
    assert!(ok, "docker buildx build failed");

    // Build the image through the real library entry point.
    let img = d.path().join("boot.ext4");
    let mut o = common::opts();
    o.journal = true;
    o.size = vmbuild::ext4::SizePolicy::FromTar {
        tar_bytes: std::fs::metadata(&tar).unwrap().len(),
    };
    vmbuild::write_ext4_from_tar(std::fs::File::open(&tar).unwrap(), &img, &o).unwrap();

    let cfg = d.path().join("fc.json");
    std::fs::write(
        &cfg,
        format!(
            r#"{{
  "boot-source": {{
    "kernel_image_path": "{}",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off acpi=off root=/dev/vda rw rootfstype=ext4 rootwait quiet loglevel=1 init=/init.sh"
  }},
  "drives": [
    {{ "drive_id": "rootfs", "path_on_host": "{}", "is_root_device": true, "is_read_only": false }}
  ],
  "machine-config": {{ "vcpu_count": 1, "mem_size_mib": 256 }}
}}"#,
            kernel().display(),
            img.display()
        ),
    )
    .unwrap();

    let out = Command::new("timeout")
        .arg("60")
        .arg("firecracker")
        .arg("--no-api")
        .arg("--config-file")
        .arg(&cfg)
        .output()
        .expect("run firecracker");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        log.contains("VMBUILD_BOOT_OK"),
        "guest did not reach init; firecracker output:\n{log}"
    );
    assert!(
        log.contains("VMBUILD_RW=yes"),
        "rootfs did not mount read-write:\n{log}"
    );

    // The guest force-rebooted while mounted rw, so the journal should have
    // pending state. `e2fsck -fp` is precisely what heyvm's grow_ext4_image
    // runs next, and it treats any code outside `& !3 == 0` as fatal.
    let st = Command::new("e2fsck")
        .arg("-fp")
        .arg(&img)
        .status()
        .unwrap();
    let code = st.code().unwrap_or(-1);
    assert!(
        code & !3 == 0,
        "post-boot e2fsck -fp returned {code}, which heyvm treats as fatal"
    );
    common::assert_fsck_clean(&img);
}
