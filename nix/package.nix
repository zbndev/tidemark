{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  git,
  clang,
  llvmPackages,
  wrapGAppsHook4,
  makeWrapper,
  gtk4,
  libadwaita,
  sqlite,
  dbus,
  hicolor-icon-theme,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tidemark";
  version = "0.2.1";
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--workspace" "--bins" ];
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
    wrapGAppsHook4
    makeWrapper
  ];
  buildInputs = [ gtk4 libadwaita sqlite dbus hicolor-icon-theme ];
  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";

  postInstall = ''
    install -Dm755 target/*/release/tidemark -t "$out/bin"
    install -Dm755 target/*/release/tidemarkd -t "$out/bin"
    install -Dm644 data/applications/io.github.zbndev.Tidemark.desktop -t "$out/share/applications"
    install -Dm644 data/metainfo/io.github.zbndev.Tidemark.metainfo.xml -t "$out/share/metainfo"
    cp -r data/icons/hicolor "$out/share/icons"
    install -Dm644 data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service -t "$out/share/dbus-1/services"
    substituteInPlace "$out/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service" \
      --replace-fail /usr/bin/tidemarkd "$out/bin/tidemarkd"
  '';

  meta = {
    description = "Track AI provider quota limits";
    homepage = "https://github.com/zbndev/tidemark";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "tidemark";
  };
})
