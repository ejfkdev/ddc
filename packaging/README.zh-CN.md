# 二进制分发打包

这个目录是仓库内二进制程序的小型多程序打包库。逻辑和 Homebrew tap /
Scoop bucket 一样：一份 catalog 列出程序，一个生成器渲染各渠道需要的清单。

`ddc` 是第一项。以后增加第二个二进制时，只需要新增一个 `[[program]]`
表；AUR、WinGet、npm 三个渲染器都是通用的。

## 渠道状态

| 渠道 | 产物 | 状态 |
| --- | --- | --- |
| GitHub Releases | 八个 target triple 的裸二进制（含静态 musl） | 已上线 |
| crates.io | `ddc-cli` | 已上线 |
| cargo-binstall | `crates/ddc-cli/Cargo.toml` 里的 `[package.metadata.binstall]` | 已上线并实测 |
| Homebrew | `ejfkdev/homebrew-tap`（独立仓库，自动更新） | 已上线 |
| Scoop | `ejfkdev/scoop-bucket`（独立仓库，自动更新） | 已上线 |
| Arch Linux / AUR | `packaging/aur/ddc-bin/` | AUR 待发布（注册临时关闭）；PKGBUILD 包已上线 |
| Windows Package Manager | `packaging/winget/` | 已合并进 winget-pkgs（#437825）；本地 zip 已上线 |
| npm | `packaging/npm/ddc/` | npmjs 已上线；GitHub Packages 已上线；tarball 已上线 |
| PyPI | — | 明确不做 |
| Docker 镜像 | — | 不在范围内 |
| Nix | — | 暂缓，原因见下 |
| Chocolatey | — | 暂缓，Windows 已由 Scoop 覆盖 |

渠道已就绪：npm wrapper 已在 npmjs 和 GitHub Packages 上线，并实际安装
运行过（包括禁用 install scripts 的场景）；WinGet 包已通过 PR #437825
合并进 `microsoft/winget-pkgs`（2026-10-30）；AUR 的 PKGBUILD 已生成并
验证，但 AUR 目前临时关闭新账号注册（HTTP 503），暂时无法提交。自包含的
`ddc-bin-<version>-1-aur.tar.gz` 仍可直接 `makepkg` 安装。npm tarball 和
WinGet 本地清单 zip 也附在每次 release 上。

**为什么不做 PyPI？** Python wheel 不适合装原生二进制：支持八个 target 就要
八个带 platform tag 的 wheel，每个都塞一份二进制，还要额外的发布任务。
npm 的 3 kB wrapper 已经覆盖了“用现有语言工具链装 CLI”的场景，PyPI
只会增加维护成本，不会带来新用户。

**为什么暂缓 Nix？** glibc 的 Linux 产物是 UPX 压缩、链接发行版 glibc 的
二进制，NixOS 需要修改 interp；静态 musl 产物可以直接运行，但正式的 Nix
包仍应在 nixpkgs 里用 `rustPlatform.buildRustPackage` + `cargoLock` 从源码
构建，那是独立于这个“预编译产物 catalog”的一项工作。

## 直接安装资产

每个 release 还附带不依赖包仓库账号的产物，现在就能用：

- **npm tarball** — `ejfkdev-ddc-<version>.tgz`：
  ```bash
  npm install -g https://github.com/ejfkdev/ddc/releases/download/v<version>/ejfkdev-ddc-<version>.tgz
  ```
- **Arch 包** — `ddc-bin-<version>-1-aur.tar.gz`：
  ```bash
  tar xzf ddc-bin-<version>-1-aur.tar.gz
  cd ddc-bin && makepkg -si
  ```
- **WinGet 本地清单** — `ejfkdev.ddc-<version>-winget.zip`：
  解压后由管理员执行一次 `winget settings --enable LocalManifestFiles`，
  然后 `winget install --manifest <解压目录>`。

`packaging/bundle.py` 生成这三种产物及 `.sha256`；`release.sh` 在清单
提交后自动上传，后续版本不需要手工操作。

## 目录结构

```
packaging/
├── programs.toml              # catalog：程序、版本、targets、渠道
├── checksums.toml             # 生成物：release 产物的 SHA-256 与大小
├── sync.py                    # 生成器 + 校验器
├── bundle.py                  # 直接安装产物（npm tarball / AUR / WinGet）
├── publish.sh                 # AUR / npm 发布，WinGet 指引
├── aur/ddc-bin/               # 生成的 PKGBUILD + .SRCINFO
├── winget/manifests/          # 生成的 WinGet 1.6.0 清单
└── npm/ddc/                   # 生成的 npm wrapper 包
```

`checksums.toml` 是生成物，不要手改；发版后跑 `sync.py --refresh`。

## Catalog

`programs.toml` 里每个二进制一个 `[[program]]` 表：

- `version_from` 指向一个 `Cargo.toml`（会跟随 workspace 版本继承）。如果
  程序不在本 workspace，改用 `version = "x.y.z"`。
- `asset` 描述 release 产物名：`{version}` 是不带 `v` 的 tag，`{target}`
  是 Rust target triple，`{exe}` 在 Windows 上是 `.exe`。
- `targets` 列出六个 release triple（未来程序可以更少）。
- `[program.aur]`、`[program.winget]`、`[program.npm]` 决定渲染哪些渠道；
  没有对应表的程序就不会生成该渠道。

增加程序：复制 `[[program]]` 块，改元数据和渠道表，然后：

```bash
python3 packaging/sync.py --refresh --program <name>
```

前提是 release API 上已经有该版本的产物。

## 发版流程

`scripts/release.sh` 在 release workflow 变绿后会自动做这几步：刷新
digest、重新生成清单、提交 `packaging/`，构建/上传直接安装产物，并触发
GitHub Packages 发布。手工等价操作：

```bash
python3 packaging/sync.py --refresh --program ddc
python3 packaging/sync.py --check
git add packaging
git commit -m "packaging: ddc <version>"
```

release 产物是唯一事实来源。`--refresh` 优先读取 GitHub 为每个 asset 提供
的 `sha256` digest（API 没有时回退到下载后计算），并拉取 tag 上的
LICENSE。渲染是确定性的，`--check` 不需要联网，直接比对字节。

## 发布

### Arch Linux（AUR）

包名是 `ddc-bin`（预编译包；`-bin` 后缀把 `ddc` 名字留给未来的源码包）。
支持 `x86_64` 和 `aarch64`，安装 `/usr/bin/ddc` 和 LICENSE，并设置
`provides`/`conflicts = ddc`。

一次性准备：

1. 注册 [AUR 账号](https://aur.archlinux.org/)。
2. 在 *My Account* 里添加 SSH 公钥。
3. 首次 push 时 AUR 会自动创建包仓库。

然后：

```bash
./packaging/publish.sh aur --yes
```

脚本会先校验生成物，再 clone
`ssh://aur@aur.archlinux.org/ddc-bin.git`（首次发布时初始化），复制
`PKGBUILD` 和 `.SRCINFO`，推送到 `master`。不加 `--yes` 只打印将执行的
操作。同样的文件以 `ddc-bin-<version>-1-aur.tar.gz` 附在每个 release 上，
可手工 `makepkg -si`。

### Windows Package Manager（WinGet）

包已通过 PR [#437825](https://github.com/microsoft/winget-pkgs/pull/437825)
合并进 `microsoft/winget-pkgs`（2026-10-30），社区清单使用 1.12.0 schema
并加了 VCRedist 依赖。`packaging/winget/manifests/e/ejfkdev/ddc/<version>/`
下生成的 1.6.0 清单用于本地 `ejfkdev.ddc-<version>-winget.zip` 和
`winget install --manifest` 路径；后来的重复 PR #466817 已关闭。

新版本用以下任一方式更新：

**wingetcreate（Windows）：**

```powershell
wingetcreate update ejfkdev.ddc --version 0.1.35 `
  --urls https://github.com/ejfkdev/ddc/releases/download/v0.1.35/ddc-v0.1.35-x86_64-pc-windows-msvc.exe `
         https://github.com/ejfkdev/ddc/releases/download/v0.1.35/ddc-v0.1.35-aarch64-pc-windows-msvc.exe `
  --submit
```

**手工 fork：** 把三个 YAML 复制到
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) fork 的
`manifests/e/ejfkdev/ddc/<version>/`，然后开 PR。Windows 上可以用
`winget validate --manifest <path>` 校验。

`./packaging/publish.sh winget` 会打印当前版本对应的准确 URL 和哈希。

### npm

包名 `@ejfkdev/ddc`，是一个 3 kB wrapper。`postinstall` 下载当前平台的
release 二进制并校验内嵌 SHA-256，`ddc` launcher 再执行原生二进制。如果
install scripts 被禁用（或 npm 拦截），launcher 会在首次运行时下载。
npm 上不带 scope 的 `ddc` 已被占用，所以使用 scope。npmjs 已发布
`@ejfkdev/ddc@0.1.33`；同一个 tarball 也附在每个 release 上，见
[直接安装资产](#直接安装资产)。

**GitHub Packages（已上线）**：
`.github/workflows/publish-github-packages.yml` 使用仓库的 `GITHUB_TOKEN`
发布同一个 wrapper，由 `release.sh` 触发。安装需要带 `read:packages` 的
GitHub token：

```bash
npm config set @ejfkdev:registry https://npm.pkg.github.com/
npm config set //npm.pkg.github.com/:_authToken "$GITHUB_TOKEN"
npm install -g @ejfkdev/ddc
```

**npmjs（已上线）**：`@ejfkdev/ddc@0.1.33` 已发布。发布脚本显式指向
`https://registry.npmjs.org/`，不会因为你本机的 mirror 配置发错仓库，并且
会先跑 `npm whoami` 检查登录。先登录：

```bash
npm login --registry=https://registry.npmjs.org/
./packaging/publish.sh npm --yes
```

无人值守发布时，创建一个带 publish 权限并勾选 **Bypass 2FA** 的 granular
token，用 `NPM_TOKEN` 传入；脚本会生成临时 0600 npmrc，退出时删除：

```bash
NPM_TOKEN=npm_... ./packaging/publish.sh npm --yes
```

发布完记得在 npmjs token 页面撤销它。普通 web 登录 token 在发布时仍会
要求 OTP。

发布前先本地测真实 tarball：

```bash
cd packaging/npm/ddc
npm pack
npm install -g --prefix /tmp/ddc-npm ./ejfkdev-ddc-0.1.35.tgz
/tmp/ddc-npm/bin/ddc --version
```

离线安装可以设置 `DDC_BINARY_PATH` 指向本地二进制。

### cargo-binstall

不需要额外文件：`crates/ddc-cli/Cargo.toml` 已有

```toml
[package.metadata.binstall]
pkg-url = "{ repo }/releases/download/v{ version }/ddc-v{ version }-{ target }{ archive-suffix }"
bin-dir = "{ bin }{ binary-ext }"
pkg-fmt = "bin"
```

`cargo binstall ddc-cli` 会直接下载裸 release 二进制。v0.1.33 已端到端
实测。

## 校验

`sync.py --check` 会逐字节比对生成物、拒绝过期的 WinGet 版本目录、对
PKGBUILD 跑 `bash -n`、解析全部 WinGet YAML、用 `node --check` 检查
JavaScript 语法、解析 `package.json`。`packaging/bundle.py` 还会构建 npm
tarball、AUR 包和 WinGet zip，并拒绝包含误下载二进制的 npm 包。
`.github/workflows/packaging.yml` 在 packaging 变更时跑同样的检查、构建
三种产物，并额外跑 `npm pack --dry-run`。

WinGet 清单在开发时还额外用官方 `manifest.version.1.6.0`、
`manifest.defaultLocale.1.6.0`、`manifest.installer.1.6.0` JSON schema
验证过。CI 保留结构检查；完整 schema 校验只需要 `jsonschema` 和三个
schema 文件，本地很容易重复。

真正的 `makepkg` 需要 Arch 环境，所以不在这个检查里。AUR 包是纯二进制
安装：一条 `install` 命令、固定哈希的 LICENSE，没有可能与 release 产物
漂移的构建步骤。

## 安全说明

- 所有清单都锁定 GitHub release API 给出的 asset SHA-256。
- npm 安装器下载后校验该哈希，不匹配就拒绝安装。
- AUR 使用同样的固定哈希，`makepkg` 会校验。
- WinGet 两个架构都带 `InstallerSha256`。
- `--refresh` 有 `GITHUB_TOKEN`/`GH_TOKEN` 时使用它，遇到限流会回退到已
  登录的 `gh` CLI。