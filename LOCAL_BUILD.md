# Local Linux Build Guide (AppImage, .deb, and `but` CLI)

This guide shows how to build GitButler for your local Linux distro and install both the desktop app and the `but` CLI.

Tested on Ubuntu/Pop!_OS 22.04 (Jammy). Adjust package commands for your distro if needed.

---

## 1) System prerequisites (Tauri + GTK/WebKitGTK 4.1)

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  curl \
  wget \
  file \
  pkg-config \
  cmake \
  libxdo-dev \
  libssl-dev \
  libgtk-3-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libwebkit2gtk-4.1-0 \
  libwebkit2gtk-4.1-dev \
  libjavascriptcoregtk-4.1-0 \
  libjavascriptcoregtk-4.1-dev \
  gir1.2-javascriptcoregtk-4.1 \
  gir1.2-webkit2-4.1
```

If you later see a FUSE error launching the AppImage, install:

```bash
sudo apt-get install -y libfuse2
```

---

## 2) Rust toolchain

GitButler pins a nightly toolchain via `rust-toolchain.toml` (currently nightly-2025-06-14). If you need Rust:

```bash
curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh
# then restart your shell
rustup show
```

---

## 3) Node + pnpm

The repo uses `pnpm@10.17.0` (declared in `package.json`). Two options:

- Option A (Corepack):
  ```bash
  corepack enable
  corepack prepare pnpm@10.17.0 --activate
  ```
- Option B (Fallback if Corepack signature errors):
  ```bash
  npm i -g pnpm@10.17.0
  ```

Then install dependencies:

```bash
pnpm install
```

---

## 4) Build GitButler (Nightly config)

This uses Tauri to produce an AppImage, `.deb`, and `.rpm` locally:

```bash
pnpm tauri build --config crates/gitbutler-tauri/tauri.conf.nightly.json
```

Artifacts will appear under:

- `target/release/bundle/appimage/*.AppImage`
- `target/release/bundle/deb/*.deb`
- `target/release/bundle/rpm/*.rpm`

---

## 5) Install the single-file app to `/usr/local/bin`

```bash
sudo install -m 0755 "target/release/bundle/appimage/GitButler Nightly_0.0.0_amd64.AppImage" /usr/local/bin/gitbutler
# Run it
gitbutler
```

Optional: install the `.deb` instead of a single-file install:

```bash
sudo apt install "./target/release/bundle/deb/GitButler Nightly_0.0.0_amd64.deb"
```

---

## 6) Create a desktop entry (launcher)

This installs a launcher pointing to `/usr/local/bin/gitbutler` and uses a stable, absolute icon path.

```bash
APPDESK="$HOME/.local/share/applications/gitbutler.desktop"
ICON_ABS="$(pwd)/crates/gitbutler-tauri/icons/128x128.png"
mkdir -p "$(dirname "$APPDESK")"
cat > "$APPDESK" <<EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=GitButler Nightly
GenericName=Git client
Comment=Git branch management tool
Exec=/usr/local/bin/gitbutler %U
Icon=$ICON_ABS
Terminal=false
Categories=Development;Utility;
Keywords=git;version control;branches;gitbutler;
StartupNotify=true
StartupWMClass=gitbutler-tauri
X-GNOME-WMClass=gitbutler-tauri
EOF
chmod 0644 "$APPDESK"

# Refresh (best-effort)
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
```

Pin to favorites on GNOME (optional):

```bash
CURRENT=$(gsettings get org.gnome.shell favorite-apps || echo "[]")
if ! echo "$CURRENT" | grep -q "'gitbutler.desktop'"; then
  if [ "$CURRENT" = "[]" ]; then NEW="['gitbutler.desktop']"; else NEW="${CURRENT%]}"; NEW="$NEW, 'gitbutler.desktop']"; fi
  gsettings set org.gnome.shell favorite-apps "$NEW"
fi
```

---

## 7) Install the `but` CLI

Build and install the CLI command to `/usr/local/bin/but`:

```bash
cargo build --release -p but
sudo install -m 0755 target/release/but /usr/local/bin/but
but --help
```

---

## Notes

- Ignore build outputs and local helpers in Git:
  - `target/`, `target/release/bundle/`
  - `apps/desktop/build/`
  - `node_modules/`, `.pnpm-store/`, `.turbo/`
  - `gitbutler` (local symlink/binary name in repo)
- See also:
  - `DEVELOPMENT.md` for dev-mode and debugging tips
  - `crates/gitbutler-tauri/tauri.conf.nightly.json` (release config we build with)
  - `package.json` scripts for dev/build tasks
