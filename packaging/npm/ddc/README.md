# @ejfkdev/ddc

Prebuilt [ddc](https://github.com/ejfkdev/ddc) binaries for macOS, Linux and
Windows. The postinstall script downloads the matching release asset and
verifies its SHA-256. If install scripts are disabled, the launcher fetches
the binary on first use instead.

```bash
npm install -g @ejfkdev/ddc
ddc --version
```

Supported platforms: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-arm64, win32-x64. Linux builds target glibc (built on ubuntu-latest); Alpine/musl users should install from source instead.

Set `DDC_BINARY_PATH` to a local binary to skip the download.
With `npm install --ignore-scripts`, the launcher fetches the binary on first
use instead.
