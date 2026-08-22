# fastcar

A production rootfs, kept here as a reference rather than a runnable example:
the Dockerfile `COPY`s the whole application, so it needs a
[fastcar](https://github.com/Heyo-Computer/fastcar) checkout as its context.

- `Dockerfile` — ~1.9 GB image: node 22, postgres, chromium, plus a Rust tool
  built in a throwaway stage. The comments are the interesting part: what each
  byte costs at cold boot, and why.
- `build-image.sh` — the wrapper fastcar ships. Drives `vmbuild build -n`,
  which installs straight into heyvm's catalog, and keeps the older
  `heyvm mvm build` path behind `UPLOAD=1` because vmbuild has no uploader.

Measured on this Dockerfile, warm docker layer cache for both:

| | `heyvm mvm build` | `vmbuild build` |
|---|---|---|
| fresh image | 94–126 s | 37 s |
| rebuild, nothing changed | 94 s | 0.5 s |
| on disk | 1919 MB | 1755 MB (sparse) |

To build it from a checkout:

```
DOCKERFILE=../fastcar/deploy/image/Dockerfile CONTEXT=../fastcar ./examples/ext4.sh
# or, what fastcar itself runs:
cd ../fastcar && VMBUILD=../vmbuild/target/release/vmbuild ./deploy/build-image.sh
```
