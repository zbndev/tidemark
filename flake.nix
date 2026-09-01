{
  description = "Tidemark desktop quota tracker";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packageFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.callPackage ./nix/package.nix { };
    in
    {
      packages = forAllSystems (
        system:
        let
          package = packageFor system;
        in
        {
          default = package;
          tidemark = package;
        }
      );

      apps = forAllSystems (
        system:
        let
          package = packageFor system;
        in
        {
          default = {
            type = "app";
            program = "${package}/bin/tidemark";
          };
          tidemark = {
            type = "app";
            program = "${package}/bin/tidemark";
          };
          tidemarkd = {
            type = "app";
            program = "${package}/bin/tidemarkd";
          };
        }
      );

      nixosModules.default = import ./nix/module.nix { inherit self; };

      checks = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          evaluated = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              { services.tidemark.enable = true; }
            ];
          };
          service = evaluated.config.systemd.user.services.tidemarkd;
        in
        assert service.wantedBy == [ ];
        assert service.serviceConfig.Type == "dbus";
        assert service.serviceConfig.BusName == "io.github.zbndev.Tidemark.Daemon";
        assert
          service.serviceConfig.ExecStart == "${evaluated.config.services.tidemark.package}/bin/tidemarkd";
        {
          nixos-module = pkgs.runCommandNoCC "tidemark-nixos-module-evaluation" { } "touch $out";
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.pkg-config
              pkgs.cmake
              pkgs.clang
              pkgs.llvmPackages.libclang
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.sqlite
              pkgs.dbus
              pkgs.desktop-file-utils
              pkgs.appstream
              pkgs.shellcheck
            ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.nixfmt-rfc-style
      );
    };
}
