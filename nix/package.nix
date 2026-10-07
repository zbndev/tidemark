{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  git,
  clang,
  llvmPackages,
  patchelf,
  fontconfig,
  libxkbcommon,
  wayland,
  vulkan-loader,
  libGL,
  xorg,
  sqlite,
  dbus,
  hicolor-icon-theme,
}:
let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  # Loaded with dlopen by the client, so no linker sees them: winit's Wayland, X11 and
  # keyboard libraries, and the Vulkan and GL loaders wgpu chooses between.
  runtimeLibraries = [
    libxkbcommon
    wayland
    vulkan-loader
    libGL
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
  ];
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tidemark";
  version = manifest.workspace.package.version;
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "--workspace"
    "--bins"
  ];
  doCheck = false;
  dontCargoInstall = true;

  installPhase = ''
    runHook preInstall
    runHook postInstall
  '';

  nativeBuildInputs = [
    pkg-config
    cmake
    git
    clang
    llvmPackages.libclang
    patchelf
  ];
  buildInputs = [
    fontconfig
    sqlite
    dbus
    hicolor-icon-theme
  ];
  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";

  postInstall = ''
    install -Dm755 target/*/release/tidemark -t "$out/bin"
    install -Dm755 target/*/release/tidemarkd -t "$out/bin"
    install -Dm755 target/*/release/tidemarkctl -t "$out/bin"
    install -Dm644 data/applications/io.github.zbndev.Tidemark.desktop -t "$out/share/applications"
    install -Dm644 data/metainfo/io.github.zbndev.Tidemark.metainfo.xml -t "$out/share/metainfo"
    install -d "$out/share/icons"
    cp -r data/icons/hicolor "$out/share/icons/"
    install -Dm644 data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service -t "$out/share/dbus-1/services"
    install -Dm644 data/tidemarkd.service -t "$out/lib/systemd/user"
    substituteInPlace "$out/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service" \
      --replace-fail /usr/bin/tidemarkd "$out/bin/tidemarkd"
    substituteInPlace "$out/lib/systemd/user/tidemarkd.service" \
      --replace-fail /usr/bin/tidemarkd "$out/bin/tidemarkd"
  '';

  postFixup = ''
    patchelf --add-rpath "${lib.makeLibraryPath runtimeLibraries}" "$out/bin/tidemark"
  '';

  meta = {
    description = "Track AI provider quota limits";
    homepage = "https://github.com/zbndev/tidemark";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "tidemark";
  };
})
